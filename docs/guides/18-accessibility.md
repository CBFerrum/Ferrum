# 18. Accessibility

What an app's interface must do so that people who use a keyboard, a screen reader, magnification, voice control,
high contrast or reduced motion can do their work; what the kit already does; what it cannot do for you; and how to
test and record it.

The target is WCAG 2.2 level AA for the shell and the kit (`docs/plan/apps-ui.md` §12.9), and the contract asks every
app for the basics: labels that name their fields, pages that fit 375px, colours that hold in dark mode
(`docs/app-contract.md`, *Reviewed*). The criteria below are written from WCAG 2.2 as published; each is mapped to the
kit components in Ferrum-Frontend (`src/ui/kit.tsx`, `src/ui/patterns.tsx`,
`src/ui/toolbar.tsx`, `src/lib/focusReturn.ts`, `src/components/ui/*`) and to what an app still has to do.

## What has and has not been tested

Say this plainly wherever your app's accessibility comes up:

- **No audit with assistive technology by a person has been done**, of the shell, the kit or any app. No screen
  reader pass, no keyboard-only walk of a whole journey, no forced-colours or touch rendering pass, no user study
  (Ferrum-Frontend: `docs/design/EVIDENCE.md`, *Not done, and why*).
- The acceptance scenarios for accessibility (UX-047 keyboard only, UX-048 assistive technology, UX-049 zoom and
  reflow, UX-050 reduced and forced display modes, UX-051 a right-to-left journey) all read `not_run`
  (`docs/design/acceptance.json`).
- What exists: contrast of every token pair measured by `src/tokens.test.ts`; axe-core in jsdom over the kit's gallery
  and some component tests (`src/test/axe.ts`; jsdom has no layout, so it checks names, roles, ARIA, labels and
  duplicate ids, never contrast or focus order); axe in Chrome over some gallery sections; a tab-order script that
  asserts focus returns to the opener for the list's overflow menu, a menu item that opens a dialog and the
  Administration dialogs (`evidence/integration-after/tab-order.json`, on the machine that did the work, not in the
  repository); the bar's nine controls checked to have names.
- One app's tests run axe: Control's `ui/src/status.test.tsx` imports the helper from Ferrum-Frontend's `src/test/axe.ts`
  by a relative path (the build refuses `@/…` imports, not relative ones). No other app's tests do, and
  `@ferrum/testing` does not export it. The server and the store look at the page of an uploaded bundle's interface
  (`ferrum_apps::ui::review`: `<html lang>`, a `<title>`, a viewport, `alt` on images, size); that is not an
  accessibility test of its screens, and the plan's "automatic accessibility pass" (`docs/plan/apps-ui.md` §10.6,
  §12.9) is not built beyond it.

So a claim that an app "meets WCAG 2.2 AA" cannot be made today. What an app can say is which of the checks below were
done, by whom, on which build, and what they found.

## What the kit gives you, and what it cannot

| Need | The kit does | You still do |
|---|---|---|
| Visible focus | one 2px outline in `--ring` with a 2px offset everywhere, kept in forced colours because it is an outline (FOUNDATIONS.md, *Focus*) | never remove `outline` without a replacement; never put focusable things under a sticky bar that hides them |
| Labels | `Field` gives a single control an id, ties the label to it, adds `aria-required`, `aria-invalid` and `aria-describedby` to the error or hint | wrap controls in `Field`; name icon-only buttons (`aria-label`); pass `labelledBy` to `RichTextEditor` |
| Dialogs | `Dialog` is a native modal `<dialog>`: focus goes in, the page behind is inert, Escape closes, focus returns to the opener, else the menu's trigger, else the page's heading, never `<body>` | give it a `title`; put a `data-autofocus` on the field to start in when the first field is not it |
| Tabs | `Tabs`: `role="tablist"`, roving `tabIndex`, arrows (mirrored right to left), Home, End | pair each with a `TabPanel` of the same `id` |
| Menus | kit `Menu`: `aria-haspopup`, `aria-expanded`, `aria-controls`; Up and Down arrows move through items, Escape closes and returns focus to the button; a `MenuItem` with `refusal` stays, is `aria-disabled` and says why | see *Menus* below for what it does not do |
| Toolbars | `Toolbar`: one tab stop, arrows, Home, End, Escape to `onEscape`; `ToolbarButton` has `aria-pressed` and `aria-keyshortcuts` | label the toolbar and each button |
| Status messages | `Toaster` is a polite live region; `announce()` speaks without showing; `StatePanel kind="failed"` is `role="alert"`, `loading` and `no-matches` are `role="status"`; `SaveStatus` is a polite status | mount `<Toaster />` once; announce the outcome of a bulk action |
| Loading | skeletons are one `role="status"` wrapper named "Loading…", shapes `aria-hidden`, pulse stops for reduced motion | use them, not a blank fallback |
| Contrast | every token pair measured: text 4.5:1, control borders, focus ring and chart series 3:1, more in high contrast | use tokens only; check your own images, charts and data colours |
| Motion | durations are 0 under `prefers-reduced-motion: reduce`, and a global rule cuts every animation and transition to 0.01ms there | do not add motion that carries meaning alone |
| Touch | 44px targets on a coarse pointer for every pressable thing | do not override `--control-h`; leave space between adjacent icon actions |
| Direction | logical properties, mirrored chevrons and arrows, `isolate()` for text shown as it is | use logical properties in your own CSS (`margin-inline-start`, not `margin-left`) |
| Wide tables | `ScrollRegion` becomes a named, focusable region only when it scrolls, so arrows reach what is cut off | give it a `label` |
| Skip link | the bar's "Skip to content" focuses `#main` (`server/shell/runtime.js`) | give your content `<main id="main">` |

What the kit cannot do: decide what your page's heading is, write good names and error messages, choose a reading
order, make a custom widget (a calendar grid, a board, a chart, a canvas) accessible, caption your media, or make a
drag only operation usable without a pointer.

## The criteria, mapped to what you do

Each heading names the WCAG 2.2 success criteria it covers (level A or AA).

### Keyboard and focus order (2.1.1, 2.1.2, 2.4.3, 2.4.7, 2.4.11)

- Everything a pointer can do, a keyboard can do: every action is a `button` or a link, never a `div` with
  `onClick`. Kit components are; a hand-made row that opens something is not. `QueueItem` with `to` makes the whole
  row one link with a focus ring.
- Focus order follows reading order. Do not use a positive `tabIndex`. A `QueueItem`'s `action` comes after its link
  in the keyboard order by design.
- No keyboard trap: a modal traps focus on purpose and lets go on Escape and close. An editor that takes Tab for
  indentation needs a documented way out (Escape to the toolbar, as `Toolbar onEscape` gives).
- Focus not obscured (2.4.11, new in 2.2): a focused control must not sit wholly under the bar, a sticky header, a
  bottom action bar or a toast. Scroll it into view with room (`scroll-margin`) when you make something sticky.
- After a route change the bar runtime is planned to move focus to the page's heading (`docs/plan/apps-ui.md` §12.2);
  in your own client-side navigations that replace the content, move focus to the new `h1` (give it `tabIndex={-1}`)
  or keep it where the person was. Not checked anywhere.
- After a dialog closes, the kit returns focus (`src/lib/focusReturn.ts`). After you remove the focused element
  yourself (a row deleted, a bulk bar gone), put focus somewhere sensible: the next row, the list, the heading.
  `SelectionToolbar` leaves this to the caller.

### Keyboard shortcuts (2.1.4)

A shortcut of a single printable key (Mail's `e` to archive, `#` to delete) must be possible to turn off or remap, or
work only when its control has focus. Mail does it right: Settings has "Keyboard shortcuts" with the hint "Turn them
off if you type into the page by accident" (`ferrum-app-comms/ui/src/MailSettings.tsx`). Never fire a single-key
shortcut while focus is in a text field. The bar keeps Ctrl/Cmd+K for its search except inside an element with
`data-ferrum-keys="mod+k"`, the only key a page can claim today (`docs/api.md`, *Keys the bar keeps*). Show shortcuts
with `aria-keyshortcuts` and in the tooltip (`ToolbarButton shortcut="Mod-b"`).

### Names, roles and values (1.3.1, 4.1.2, 2.5.3, 2.4.6)

- Every control has an accessible name. Icon-only buttons need `aria-label`: `Menu ariaLabel={t("More actions")}`,
  `<button aria-label={t("Close")}>`. A visible label is part of the name (2.5.3, label in name): a button showing
  "Archive" must not be named "Move to archive folder", or voice control users cannot say what they see.
- Headings form an outline: one `h1` per page (`PageHead` draws it), `h2` for sections, no skipped levels for
  looks. `Queue` sections are named by their heading.
- Landmarks: the bar is outside your page; your content is `main`. `PageSplit`'s aside is a plain `div` on purpose.
  Do not add `role="region"` to everything; a region needs a name.
- State is told, not only shown: `aria-pressed` for toggles, `aria-expanded` for disclosures (the kit's fold of
  empty fields is a button with `aria-expanded`), `aria-selected` for tabs, `aria-current="page"` for the current
  navigation item, `aria-busy` while a button works (`Button loading`).
- Decorative icons are `aria-hidden` (the kit's `Icon`, `NavIcon` and the state icons are).

### Forms: labels, instructions, errors (1.3.1, 3.3.1, 3.3.2, 3.3.3, 3.3.4, 3.3.7)

```tsx
<Field label={t("Due date")} required error={errors.due} hint={t("The day the customer must pay by.")}>
  <input type="date" className="input" value={due} onChange={...} />
</Field>
```

- Every input has a visible label (`Field label`). A placeholder is not a label.
- Required is said in words to assistive technology (`aria-required`); the visible `*` is `aria-hidden`.
- An error names the field and what to do ("Enter a date after the start date"), sits under the field
  (`aria-describedby`), and the control is `aria-invalid`. The server's 422 `fields` come back in `ApiError.fields`
  with `row` for a child row: show each at its field. The records frame's form does this, with an error summary that
  names each field (EVIDENCE.md, *The craft pass*).
- Do not clear what the person typed on an error, and keep a draft through navigation and failure.
- For anything with legal or financial weight, or that deletes data (3.3.4), the person can check and confirm
  before it is done, or undo it: `confirmDialog({ danger: true, … })`, or an Undo toast for at least 8 seconds.
- Redundant entry (3.3.7, new in 2.2): do not ask for something the person already gave in the same process
  (prefill the billing address from the shipping one, or offer "same as").
- Accessible authentication (3.3.8, new in 2.2): no puzzle or memory test to sign in. The step-up dialog offers a
  passkey, the password or the identity provider (`ConfirmItsYou`); do not add a step that asks people to retype a
  code you could let them paste.

### Contrast and colour (1.4.1, 1.4.3, 1.4.11)

- Text 4.5:1 and large text 3:1 (1.4.3); control boundaries, focus indicators and the parts of a chart that carry
  meaning 3:1 (1.4.11). The tokens meet these in light, dark and high contrast, measured by `src/tokens.test.ts`
  (FOUNDATIONS.md, *Measured contrast*). Your own colours (data colours, images, charts) are yours to measure.
- Colour is never the only signal (1.4.1): a status is a word on a badge with its tone; `SaveStatus` is an icon and
  words; a pressed `ToolbarButton` has a fill and a mark; a negative figure in a report has its minus sign as well as
  `--danger`; an invalid field has its message, not only a red border. A chart series needs a label or pattern, not
  only a hue; a legend alone is not enough when the series cross.
- A workspace's brand accent is adjusted by `lib/theme.ts` until it passes 4.5:1 with its text.

### Target size (2.5.8) and pointer gestures (2.5.1, 2.5.2, 2.5.7)

- 2.5.8 asks for at least 24 by 24 CSS pixels, or enough space around a smaller target. The kit's smallest control is
  `Button size="xs"` at 24px; on a coarse pointer everything is 44px. A 16px icon link in a dense row needs its
  padding to reach 24.
- Dragging (2.5.7, new in 2.2): every drag has a single-pointer or keyboard way too. Drive moves with a Move dialog as
  well as drag; the record frame's child table moves a row with Alt+Up and Alt+Down; the calendar's events move by
  dragging and are changed in their form. A board column change needs a menu or a field too.
- Multipoint and path gestures (2.5.1) need a one-finger alternative (zoom buttons beside pinch).
- Pointer cancellation (2.5.2): act on release, not on press, unless it is a key-like control. The kit's `useDismiss`
  closes a menu on release for touch and pen.

### Motion and animation (2.2.2, 2.3.1, 2.3.3)

The kit's durations are zero under reduced motion and a global rule in `src/styles.css` cuts animation and
transition everywhere. Do not start motion that lasts more than five seconds without a pause (a carousel, a ticker), do
not flash, and do not rely on an animation to tell something happened. Spinners stop under reduced motion
(`motion-reduce:animate-none` in `SaveStatus`). Do not use a scroll-driven animation to show state: the
edge cue of a scrolling row is set from the scroll position by `watchEdges()` (PATTERNS.md, *Scrolling rows*).

### Zoom, text size and reflow (1.4.4, 1.4.10, 1.4.12, 1.3.4)

- The root is 87.5% and every token is in rem, so the person's own font size setting scales the whole interface.
  Never set the root or a size in px for text; never block zoom (lint `zoom-lock`).
- Reflow (1.4.10): at 320 CSS pixels wide (400% zoom of a 1280 window) content reads in one column without sideways
  scrolling, except what is two-dimensional by nature (a wide table, a spreadsheet, a map), which keeps a way to
  reach every part (`ScrollRegion`). The 375px rule of the contract and the kit's phone layouts cover most of it;
  check a page of your own at 320.
- Text spacing (1.4.12): nothing is cut or overlaps when line height is 1.5, paragraph spacing 2, letter spacing 0.12
  and word spacing 0.16 of the font size. Avoid fixed heights on text containers.
- Orientation (1.3.4): do not lock to portrait or landscape.

### Tables and grids (1.3.1, 1.3.2)

- A table of data is a `<table>` with `<th>` headers (the kit's `Table` primitive, `.table`); a row header is `<th
  scope="row">`. Do not build a table from `div`s.
- Figures are end-aligned and tabular; a total row is marked by its words and rule, not only by weight.
- An interactive grid (a spreadsheet, the child-table line grid) follows the grid pattern: one tab stop, arrows
  between cells, Enter to edit, Escape to leave. Sheets is the editors' (`acceptance.json` UX-035, not run); the
  records frame's line grid is the kit's. If you need a new grid, ask for a governed one in the kit.
- Sorting is a button in the header with `aria-sort` on the header cell.

### Dialogs and menus

- Use `Dialog`, `confirmDialog` or the Radix `Dialog`, `AlertDialog`, `Sheet` from primitives; never a `div` that
  looks like one. A confirm opened over another dialog or sheet works (PATTERNS.md, *A confirm over another layer*).
- Menus: the Radix `DropdownMenu` (primitives) follows the menu pattern fully: focus moves into it on open, arrows,
  Home, End, type-ahead, Escape returns focus. The kit's own `Menu` (used where a page must not load Radix) differs:
  focus stays on the button when the menu opens until Down or Up moves into it; there is no Home, End or type-ahead;
  each item is a tab stop rather than one roving stop; Tab does not close it. It returns focus to the button on Escape
  and on choosing. Prefer the primitive in a new screen with more than a few items, or extend the kit's `Menu` in the
  kit.
- A menu row that cannot be chosen says why (`MenuItem refusal`) rather than disappearing.

### Live regions and status messages (4.1.3)

- A change that happens without a page load and matters to the person is announced: a save's outcome (`SaveStatus`),
  a bulk action's result (`announce(t("3 conversations moved to Trash"))`), a filter that leaves nothing
  (`StatePanel kind="no-matches"` is a status), a failure (`kind="failed"` is an alert).
- Toasts announce politely and never carry the only copy of an error: an error that matters stays on the page, at the
  field or in a `StatePanel` (PATTERNS.md, *Overlays*).
- Do not announce on every keystroke or every live update; debounce, and announce the summary.
- `announce` speaks once; pair it with a toast only when the toast has no words of its own.

### Time limits (2.2.1)

- Toasts with an action stay at least 8 seconds and every toast's timer stops while the pointer is over it or focus
  is inside it (`ToastView` in `src/ui/kit.tsx`); error toasts stay 9 seconds. Do not shorten them.
- A session that ends is signed in again by the bar, and the client sends refused requests again after a step-up; do
  not throw away a form when that happens.
- An offline lease ends after the workspace's `hours` (`docs/offline.md`); work not sent yet is kept regardless.
- A countdown that moves is not drawn (`TargetCard`: "Do not … draw a countdown that moves").

### Right to left and language of parts (1.3.2, 3.1.1, 3.1.2)

- The page's `lang` and `dir` come from the bootstrap (`applyLocale` sets them). Do not set `dir` on your own root.
- Text shown as it is (a name, a code, a count built in code) in a right-to-left page goes through `isolate()`, so
  its punctuation and numbers keep their order; `t()` isolates English left untranslated in such a page
  ([19-languages-and-formats.md](19-languages-and-formats.md)).
- Arrows that mean "along the line" are mirrored (`Icon` with `chevronRight` or `left` gets `icon-dir`; the kit's tabs
  and toolbars swap Left and Right for the page's direction, and a key handler of your own must too).
- A passage in another language than the page (a quoted mail) gets its own `lang` where you know it.

### Media (1.2.1 to 1.2.5)

Recorded video needs captions and a transcript or audio description; live video (Meet, Live) needs captions for AA
(1.2.4). The kit has no media player or caption support, and no app checked here provides captions. An app that adds
recorded or live media owns this, and must say in its README that captions are not provided until they are.

### Consistency and help (3.2.3, 3.2.4, 3.2.6)

The same thing is named and placed the same way on every page: the kit's page anatomy gives one title place, one
action row and one overflow menu. Help (3.2.6, new in 2.2): if your app offers a way to get help, it sits in the same
place on every page.

## How to test

Do these on a running server with your app built (`02-your-first-app.md` says how to run one), and record what you
did. Each takes minutes per screen.

1. **Automated.** There is no axe step in an app's `cli.mjs` commands (above). Until there is, run the axe browser extension
   or Lighthouse's accessibility audit in Chrome on each screen, in light and dark, and keep the report. It finds
   perhaps a third of real problems: missing names, bad ARIA, contrast of text it can see.
2. **Keyboard only.** Unplug the mouse. From the bar, Tab into the page (use "Skip to content"), do the screen's main
   task to the end, open and close every dialog and menu, and check: you can reach everything, you can always see
   where focus is, focus never lands behind something, Escape closes what it should, focus comes back where you were.
3. **Screen reader.** VoiceOver with Safari on a Mac (Cmd+F5), NVDA with Firefox or Chrome on Windows, TalkBack
   with Chrome on Android. Go through the page by headings, then by form controls, then do the task. Listen for:
   every control named for what it shows, every state change and error spoken, nothing read twice, nothing
   meaningless ("button", "image", an id). This is the check that has never been done on Ferrum; say who did it and
   with what.
4. **Zoom and reflow.** Browser zoom to 200% and to 400% on a 1280px window (about 320px wide): nothing cut off,
   no sideways scrolling except in tables and editors, nothing overlaps. Then the browser's minimum font size at
   24px.
5. **Reduced motion and contrast.** Turn on reduced motion in the operating system; turn on increased contrast
   (the operating system's, which the page follows when the person's appearance says "system", or "more" in their
   own appearance setting: `lib/theme.ts` sets `data-contrast` from either) and, on Windows, a forced-colours theme:
   focus still visible, selected and invalid states still distinguishable.
6. **Phone.** 375px wide with touch emulation, then a real phone: targets big enough, nothing under the on-screen
   keyboard that you need.
7. **Right to left.** The page in Arabic (the person's language preference): layout mirrored, numbers and codes
   intact, arrows pointing the right way.

## How to record it

Per screen, in your repository's `docs/` (a file such as `docs/accessibility.md`) or in the release's notes: the app
version and the Ferrum-Frontend commit it was built from (`ui/dist/.source` names it), the date, the browser and
assistive technology with versions, the role and data used, each check above as done or not done, and each problem
found with what was fixed. If the screen is covered by an acceptance scenario, record the result there in the form
`ACCEPTANCE.md` asks for. Do not write "accessible" or "WCAG compliant" in a README without that record; write what
was checked.

## A checklist for each screen

- [ ] One `h1`; headings in order; content in `<main id="main">`.
- [ ] Every control reachable and usable by keyboard; no positive `tabIndex`; no trap.
- [ ] Focus always visible and never hidden under the bar, a sticky header, a bottom bar or a toast.
- [ ] Every control named; icon-only buttons have `aria-label`; the visible words are in the name.
- [ ] Every input in a `Field` with a visible label; required, errors and hints tied to it.
- [ ] Errors say what is wrong and how to fix it, at the field; the person's input is kept.
- [ ] Destructive or binding actions confirm or offer Undo for at least 8 seconds.
- [ ] Only tokens for colour; status shown in words as well as tone; charts readable without colour.
- [ ] Targets at least 24px on desktop; 44px on touch (kit default).
- [ ] Every drag has another way; single-key shortcuts can be turned off.
- [ ] Loading, empty, no matches, denied, offline and failed states each drawn and each announced as the kit does.
- [ ] Bulk and background outcomes announced (`announce`, `SaveStatus`).
- [ ] Works at 320px wide and at 200% zoom; nothing in px for text; zoom not blocked.
- [ ] Reduced motion leaves nothing moving; nothing flashes.
- [ ] Right to left: mirrored, `isolate()` on text shown as it is, logical CSS properties.
- [ ] Media: captions provided, or the README says they are not.
- [ ] What was checked, by whom and with what, recorded.

## Checked by

- Ferrum-Frontend `src/tokens.test.ts` (contrast of the tokens), `src/test/axe.ts` used by the kit's own component
  tests and the gallery's test (jsdom), and the UI lint's `small-text`, `arbitrary-text-size` and `zoom-lock` rules
  (`packages/build/uilint.mjs`), which also run in every app's `cli.mjs check`.
- Nothing checks an app's own screens for accessibility automatically. Reviewed, not checked: everything in the
  checklist above.

## See also

- [17-the-interface.md](17-the-interface.md) for the kit and its states; [19-languages-and-formats.md](19-languages-and-formats.md)
  for direction; [20-writing-for-people.md](20-writing-for-people.md) for names, errors and confirmations
- `docs/app-contract.md` (*Reviewed*), `docs/plan/apps-ui.md` §12.2 and §12.9
- Ferrum-Frontend: `docs/design/FOUNDATIONS.md` (*Focus*, *Measured contrast*, *Motion*), `PATTERNS.md`
  (*Overlays*, *StatePanel*, *Toolbar and swatch palette*), `EVIDENCE.md` (*Not done, and why*), `acceptance.json`
  (UX-047 to UX-051)
- WCAG 2.2 (W3C Recommendation) and the WAI-ARIA Authoring Practices for the tabs, menu, toolbar, dialog and grid
  patterns
