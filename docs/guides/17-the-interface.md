# 17. The interface

Each app owns its interface. It lives in `ui/` of the app's own repository, is built with Ferrum-Frontend's
packages, and is served by the platform on the workspace's host with the platform's bar above it. This guide says how
that works, which parts of the kit to use for what, and what is checked before an interface ships.

Ferrum-Frontend is another repository (the platform's `Dockerfile` pins the revision it builds as `FRONTEND_REV`); its paths are written as
`Ferrum-Frontend: <path>`. Its design documents are the rules for the look: `docs/design/README.md`,
`FOUNDATIONS.md`, `PATTERNS.md`, `MIGRATION.md`, `EVIDENCE.md` and `ACCEPTANCE.md`. This guide points into them and
does not copy them.

## How an interface reaches a person

```
ferrum-app-<name>/ui/         your sources (Vite, React, TypeScript)
        |  node ../../ferrum/web/packages/build/cli.mjs build
        v
ferrum-app-<name>/ui/dist/    built files + dist/.source (digest of the sources) + dist/words.json
        |  committed; the app's crate embeds it at compile time (its build.rs)
        v
the server                    registers it at start as the interface release of that app version
        |  GET /<route>/...   -> index.html with the bootstrap, the theme and the bar runtime put in
        v
the browser                   <ferrum-bar> on top, your app below, calling /api/... with the session cookie
```

- **Where the sources sit.** `ui/` beside `app.yaml`, built against a Ferrum-Frontend checkout at
  `../../ferrum/web` from `ui/` (the platform checkout beside the app, with Ferrum-Frontend inside it as `web/`). The
  `@ferrum/*` packages are not on npm; `packages/build/vite.mjs` resolves every library an app imports as
  Ferrum-Frontend's own, so the app and the kit share one React, one router and one query client
  (Ferrum-Frontend: `packages/build/vite.mjs`, `resolveLikeTheKit`). An app imports `@ferrum/*` only; an import of
  Ferrum-Frontend's own modules (`@/…`) fails the build with "an app imports the @ferrum/* packages, never
  Ferrum-Frontend's own modules".
- **The manifest.** `ui.dist: ui/dist` and the `contributes.shell.app` entries that give it routes
  ([03-the-manifest.md](03-the-manifest.md); `docs/plugins.md`, *Interfaces of their own*; `docs/plan/apps-ui.md`
  §10.1). Routes match `[a-z][a-z0-9-]{1,30}` and never a reserved name (`api assistant ws files mcp metrics oauth open data
  admin store account portal public accounts login logout s app home sw.js robots.txt favicon.ico manifest.webmanifest`,
  anything starting with `_` or `.`).
- **The page.** The server answers `/<route>/…` with your `index.html` and puts in, before your scripts: the
  bootstrap (`<script type="application/json" id="ferrum-bootstrap">`), the workspace's theme and the bar runtime
  (`/_shell/v1/runtime.js`), which draws `<ferrum-bar>` and installs `ferrum.shell` (`docs/plan/apps-ui.md` §10.2,
  §12.2). Your `index.html` has one head and one body, module scripts of its own files only, no inline script, no
  `<base>`, no remote script or style, and no Content-Security-Policy of its own (`docs/plugins.md`).
- **Files and caching.** A release is stored under `ui/{app}/{digest}/` and served at `/_ui/{app}/{digest}/…`,
  immutable, kept 30 days after no workspace uses it (`docs/operations.md`, *Apps with interfaces of their own*).
- **Trust.** An interface compiled into the server, signed by a trusted key or made available to the whole cluster
  runs on the workspace's host with the person's session. An unsigned upload to one workspace runs isolated in a
  sandboxed frame (`docs/plugins.md`, *Isolated interfaces*). Everything below is about the first kind.

## The packages

| Package | What you take from it | Source |
|---|---|---|
| `@ferrum/client` | `bootstrap()`, `shell()`, `api` and `request()` (the session, retries, step-up, version skew), `ApiError`, `errorMessage`, `refusal`, `t()`, `isolate()`, `addCatalogs()`, `wordsReady()`, `formatMoment`, `formatDecimal`, `formatValue`, `relativeTime`, `routeBase()`, uploads (`uploadFile`, `uploadInParts`), the offline names (`cacheName`, `pendingName`, `sweep`) | Ferrum-Frontend: `packages/client/src/index.ts` |
| `@ferrum/kit` | components and patterns: `Button`, `Badge`, `StatusBadge`, `Field`, `Dialog`, `confirmDialog`, `Menu`, `Tabs`, `Alert`, `Toaster`, `toast`, `announce`, `StatePanel`, `LoadFailed`, the skeletons, `SaveStatus`, `SelectionToolbar`, `PageHead`, the work-home pieces, `Toolbar`, `RichTextEditor`, `NavIcon`, `AppIcon`, `useLiveEntity`, `useLivePush`, `useUnsavedGuard` | `packages/kit/src/index.ts` |
| `@ferrum/kit/primitives` | the shadcn building blocks (`Button`, `Dialog`, `Sheet`, `DropdownMenu`, `Select`, `Table`, …) for screens that compose their own | `packages/kit/primitives/src/index.ts` |
| `@ferrum/records` | lists, forms and reports of any entity from its metadata (`ListView`, `FormView`, `ReportView`), people, comments, the record picker | `packages/records/src/index.ts` |
| `@ferrum/records/frame` | the records frame and an app's start: `mountApp`, `setFrameMode`, `RequireAuth`, `recordsRoutes`, `createAppRouter` | `packages/records/frame/src/index.ts` |
| `@ferrum/testing` | `fakeBootstrap()`, `installFakeShell()` for tests | `packages/testing/src/index.ts` |
| `@ferrum/build` | `cli.mjs build`, `check`, `lint`, `dev`, `typecheck`, `test`; `vite.mjs` | `packages/build/` |

Use what the kit exports. If it lacks a pattern, add a governed one to the kit; do not fork `Button`, `Menu` or
`Dialog` in an app, and do not import `radix-ui`, `@radix-ui/*` or `class-variance-authority` there: the lint's
`forked-primitive` rule fails it (Ferrum-Frontend: `packages/build/uilint.mjs`). The order of preference is kit
components, then primitives, then the legacy class names (`.btn`, `.input`, …) only where existing screens use them
(Ferrum-Frontend: `docs/design/MIGRATION.md`, *What to use*).

## The files of `ui/`

Copied from Drive (`ferrum-app-drive/ui`), which is the fullest example; Code (`ferrum-app-code/ui`) is the smallest
one that uses the records frame.

`ui/vite.config.mjs` (plain JavaScript: `cli.mjs` loads it with Node itself):

```js
import { ferrumApp } from "../../ferrum/web/packages/build/vite.mjs";
export default ferrumApp(import.meta.url);
```

`ui/package.json` names the commands. The scaffold's template (`ferrum-sdk/templates/ui/native/package.json`) has
`build`, `check`, `dev`, `typecheck` and `test`; most apps omit `dev` and run the CLI directly:

```json
"scripts": {
  "build": "node ../../ferrum/web/packages/build/cli.mjs build",
  "check": "node ../../ferrum/web/packages/build/cli.mjs check",
  "typecheck": "node ../../ferrum/web/packages/build/cli.mjs typecheck",
  "test": "node ../../ferrum/web/packages/build/cli.mjs test"
}
```

`ui/tsconfig.json` extends `../../ferrum/web/packages/build/tsconfig.app.json`. `ui/src/styles.css` must start with
the kit's base, or the page ships unstyled (`cli.mjs check` fails it: "the built CSS lacks the kit's base
stylesheet"):

```css
@import "../../../ferrum/web/src/index.css";
@source "../../../ferrum/web/src";
@source ".";
@import "./drive.css";
```

`ui/src/main.tsx` (Code's, shortened): `mountApp` adds the app's catalogs, builds the router at the app's address,
draws once the words are in, and puts the toasts and confirmations beside the page. Call it once, last, after
`setFrameMode` when the app has the frame.

```tsx
import { shell } from "@ferrum/client";
import { mountApp, RequireAuth, setFrameMode } from "@ferrum/records/frame";
import "./styles.css";

setFrameMode("app", ["code"]);

mountApp({
  route: "code",
  catalogs: { ar: () => import("./locales/ar.json"), hi: () => import("./locales/hi.json") },
  routes: [{ path: "/", element: <><Started /><RequireAuth /></>, children: [...] }],   // Started calls shell().ready()
});
```

What each line is for:

- `route: "code"` is read through `routeBase("code")`, the app's address from the bootstrap. Never hard-code `/code/`:
  an operator may remap a route (`docs/plan/apps-ui.md` §4 rule 2, §10.1).
- `catalogs` are the app's own words ([19-languages-and-formats.md](19-languages-and-formats.md)); `mountApp` waits for
  the page's language and these (`wordsReady()`) before it draws, so English does not show for a moment.
- `Toaster`, which `mountApp` draws, holds the polite live region that `toast()` and `announce()` speak through; without
  it they say nothing ([18-accessibility.md](18-accessibility.md)).
- `ConfirmHost`, which `mountApp` draws, draws `confirmDialog()` and the step-up dialog: the client asks the person to confirm it is them
  when the server answers `403 step_up`, and sends the request again once (Ferrum-Frontend:
  `packages/client/src/index.ts`, `send`). Without it, a step-up refusal is just an error.
- `shell().ready()` once the first page has drawn. If an app never calls it within ten seconds the runtime reports
  that it did not start (`docs/plan/apps-ui.md` §10.2). Code calls it from a `Started` element in its root route;
  Drive from its `Frame`.
- `mountApp` makes one query client: no retries, no refetch on window focus, data fresh for 30 seconds. An app that
  needs other behaviour starts itself with `createAppRouter` and a client of its own.
- Pages other than the home load when opened (`lazy` routes). The entry JavaScript budget is 200 KB compressed and
  CSS 50 KB (Ferrum-Frontend: `packages/build/budgets.mjs`).
- Give the content a `<main id="main">`: the bar's "Skip to content" link focuses `#main`, else the first `main`
  (`server/shell/runtime.js`, `skip`).

## The shell from an app

`shell()` from `@ferrum/client` returns the host's API or a fallback that does something sensible where no shell
runs (a test, a page opened alone). The capabilities and their fallbacks are the table in `docs/plan/apps-ui.md`
§10.3; the TypeScript shape is the `Shell` interface in Ferrum-Frontend `packages/client/src/index.ts`.

| Call | Use it for |
|---|---|
| `ready()` | once, after the first draw |
| `navigate(path \| {entity, id} \| {path})` | another app's page or a record: the host decides how |
| `title(text)`, `breadcrumbs(trail)` | the bar's title and trail (`useShellTitle` from the kit sets the title while a screen shows) |
| `intent({action, type, data})`, `canIntent(action, type)` | ask another app to do something (pick a file, compose a mail); `useCanIntent` hides the button when nobody handles it |
| `leaveGuard(isDirty)` | ask before leaving with unsaved changes; the kit's `useUnsavedGuard` covers the router's own navigations |
| `live.subscribe(topics, onChange, {resync})` | live updates (below) |
| `uploads.start`, `uploads.track` | transfers that outlive the page, shown in the bar's tray; an app does not draw its own upload panel where `uploads.canTrack` is true |
| `toast(message, {tone})` | a message in the host's layer; inside your page the kit's `toast()` is the one to use |
| `assistant(context)` | open the assistant with what the person is looking at |
| `badge(count)` | the app's unread count on the launcher |
| `sidepanel.open`, `close`, `current`, `context`, `list` | the panels the bar opens in any app |
| `companion({kind, url})` | a window that outlives page loads (a call) |
| `session.signOut()`, `reauthenticate()`, `on(event, cb)` | the person's session |
| `family()`, `apps()` | the family's entries (the kit's `FamilyNav` draws them) and the launcher's entries |
| `supports(capability)` | ask before using anything newer; never compare version numbers (`docs/plan/apps-ui.md` §4 rule 3) |

Hiding is not securing: a launcher entry, a route or a hidden button is convenience; the API checks every call
(`docs/plan/apps-ui.md` §4 rule 5; [05-permissions-and-people.md](05-permissions-and-people.md)).

## The records frame

Most apps do not draw lists and forms of their own entities. `@ferrum/records` draws them from metadata, so a field a
package or a workspace adds appears without an interface change, and lists, forms, actions, history, comments and
permissions behave the same in every app (`docs/app-contract.md`, *Reviewed*: "Record pages for what people work
with").

- `setFrameMode("app", packages)` puts the app's modules in the left menu under the bar; `RequireAuth` around the
  routes; `recordsRoutes` gives every records route (lists at `/e/:entity`, forms at `/e/:entity/:id`, reports at
  `/r/:name`) and `setFrameMode("app", packages, { pages: true })` says the frame has them. CRM
  (`ferrum-app-crm/ui/src/main.tsx`, `routes.tsx`) mixes its own pages (the pipeline, a deal at `/d/{id}`) with the
  frame's lists and forms.
- `createAppRouter(routes, { basename })` instead of `createBrowserRouter`: it adds the wait page React Router wants
  on the first load (Ferrum-Frontend: `packages/records/frame/src/index.ts`). `mountApp` uses it.
- On a phone the records frame turns lists into cards with a Sort select, keeps a form's actions in a bar at the
  bottom, shows child tables as one card per row and opens dialogs as bottom sheets (Ferrum-Frontend: `README.md`,
  *Phones*).
- Money and decimals stay strings end to end. The interface never adds them up: totals come from
  `POST /api/v1/{Entity}/_calc`, or from an engine built for WebAssembly where the page must price offline (Point of
  Sale vendors ERP's pricing crate, `ferrum-app-pos/ui/src/lib/pricing.ts`; `@ferrum/calc` is the platform's).
  Saves send `If-Match`; creates send an `Idempotency-Key` (Ferrum-Frontend: `README.md`, *Conventions*).

## The look: tokens, type, spacing, status

The values live in one file, Ferrum-Frontend `src/index.css`; `src/tokens.test.ts` measures their contrast. Rules
for an app:

- **Colour comes from semantic tokens**, never from hex, `rgb()`, `hsl()` or `oklch()` in your CSS or class names.
  The roles: `--bg`, `--surface`, `--surface-2`, `--surface-3`, `--text`, `--muted-foreground`, `--faint`,
  `--border`, `--border-strong`, `--border-control`, `--primary` (and `-hover`, `-soft`, `-text`), `--success`,
  `--warning`, `--danger`, `--purple` (each with `-soft`), `--chart-1` to `--chart-8` (FOUNDATIONS.md, *Colour
  roles*). The lint's `raw-color` rule finds literals. A colour that is data (a person's chosen colour, a label's
  colour, a document's palette) is allowed with a reason on the line or the line before:
  `// ui-lint-allow raw-color: a user's own colour is data`.
- **Text sizes are the type roles**: `text-xs` 12/16 caption, `text-sm` 13/18 compact UI, `text-base` 14/20 standard,
  `text-lg` 16/24 reading, `text-xl` 18/24 section heading, `text-2xl` 24/32 page heading; `text-3xl` and
  `text-4xl` only for the one figure a screen is about. Never `text-[13px]` (`arbitrary-text-size`) and nothing under
  12px (`small-text`, which also reads `font-size` in CSS and `fontSize` in TSX).
- **Spacing is a true 4px**: `--spacing` is `calc(4 / 14 * 1rem)` under the 87.5% root, so `p-4` is 16px and `h-9`
  36px. Use the steps 4, 8, 12, 16, 24, 32, 48 (PATTERNS.md, *Page anatomy*). Do not change the root font size.
- **One control height per row**: controls take `--control-h` (36px, 32 compact, 44 on a coarse pointer) and
  `--control-h-sm` (32px, 36 on touch) (FOUNDATIONS.md, *Geometry and density*).
- **Status tone comes from a stable state id, never from the label.** `StatusBadge value={doc.status} tones={...}`
  takes the tone from the stored value; `statusTone` gives a tone only to well-known whole ids and neutral to
  anything else, translated text included (Ferrum-Frontend: `src/ui/kit.tsx`). Code keeps its own map from state id
  to words and tone (`ferrum-app-code/ui/src/words.ts`) and tests that an unknown state shows as it is
  (`words.test.ts`).
- **Radii, shadows, motion**: `--radius` 8px panels, `--radius-sm` 6px controls; only floating things (menus,
  popovers, dialogs, sheets, toasts) cast `--shadow`; durations `--duration-state`, `--duration-overlay`,
  `--duration-panel`, all 0 under reduced motion.
- **Authored content keeps its own styling**: a document body, a print template, slide and sheet content follow the
  document, not the interface's theme or type roles (FOUNDATIONS.md, *What must not use UI tokens*).

## Page anatomy and patterns

Follow PATTERNS.md; the parts an app meets first:

| Need | Use | Notes |
|---|---|---|
| A page's head | `PageHead` (also exported as `PageHeader`) | one `h1` per page, sentence case; at most one primary action; the rest default buttons; rare ones in one "More actions" menu |
| An app's home | `PageHead`, `QueueGrid` of `Queue`s of `QueueItem`s, `TileGrid` of `Tile`s, `StatGrid` of `Stat`s, `HomeSkeleton` | PATTERNS.md, *Work home*: answers "what needs me now?"; the count goes in `count`, never in the title |
| Sections of a page | `PageStack gap="md" \| "lg"`, `PageSplit main aside`, `HomeSection` | do not wrap content in `grid gap-4`; cards do not nest |
| A form field | `Field label required error hint` | gives a single control an id, `aria-required`, `aria-invalid`, `aria-describedby` |
| A dialog | `Dialog` (native `<dialog>`), `confirmDialog({title, message, confirmLabel, danger})` | footer right-aligned, secondary first, primary last |
| A menu of actions | `Menu`, `MenuItem refusal="why"` | an item that cannot be chosen stays and says why |
| Tabs | `Tabs`, `TabPanel` | WAI-ARIA tabs with arrow keys |
| Rows selected | `SelectionToolbar` with `actions=[…]` | folds what does not fit into "More"; says the scope ("All 240 matching selected") |
| A row of tools over content | `Toolbar`, `ToolbarGroup`, `ToolbarButton` | one tab stop, arrows move along it |
| Rich text | `RichTextEditor`, shown with `RichText` | never set HTML yourself; `sanitizeRichText` mirrors the server's allow-list (`crates/kernel/src/richtext.rs`) |
| A meter | `Progress label="…"` | never a hand-drawn bar |
| A service target | `TargetCard` | the state is a word on a badge; no moving countdown |
| A wide table | `ScrollRegion label="…"` | becomes a focusable, named region only when it scrolls |

The task layouts (inbox, file browser, editor, record workspace, builder, analytics) are compositions of these, not
components (PATTERNS.md, *Task layouts*). Not built yet in the kit: a context inspector, an approval preview, a
transfer tray, an app header and view toolbar as separate components, a stale banner (PATTERNS.md, *Not built
yet*). Use what the section says to use until then.

## States: loading, empty, no matches, denied, offline, failed, stale

Keep them apart, each with its own look and words (PATTERNS.md, *State → what the user sees → component*):

| State | Component |
|---|---|
| Loading, shape known | `ListSkeleton`, `TableSkeleton`, `FormSkeleton`, `ReaderSkeleton`, `HomeSkeleton` |
| Loading a route | `PageLoading` |
| Loading, nothing to shape | `StatePanel kind="loading"` |
| First use, nothing yet | `StatePanel kind="empty"` with the first step as its `action` |
| A filter or search hid everything | `StatePanel kind="no-matches" onClear={…}` |
| Not allowed | `StatePanel kind="denied"`: never says what the thing is or whether it exists |
| No connection | `StatePanel kind="offline"`; claim work is kept only if your code keeps it |
| A request failed | `LoadFailed error={q.error} onRetry={() => q.refetch()}`: it picks denied for 401 and 403, offline when there was no answer, failed otherwise |
| Content is old | keep it; a kit `Alert` with a refresh `action` |
| A change in flight | `SaveStatus state="draft" \| "saving" \| "saved" \| "local" \| "conflict" \| "failed" \| "unknown"` |

Mistakes the lint and the reviews catch:

- `fallback={null}` (or an empty `<span/>`, `<div/>`, `<></>`) on `Suspense` is the lint's `blank-fallback`.
- An empty state where a filter hid the rows: that is `no-matches`.
- A refused page drawn as an empty one (EVIDENCE.md, *Found by looking, fixed*). A 403 is `denied`, not "No records
  yet".
- "Saved" before the server confirmed it; a draft lost on failure or conflict. Never show save, send, payment or
  approval as done before the durable milestone (Ferrum-Frontend: `CLAUDE.md`, *Hard rules*).
- A hand-made Undo bar: every destructive or filing action offers Undo for at least 8 seconds through
  `toast(message, tone, { action: { label: t("Undo"), onSelect } })` (PATTERNS.md, *Overlays*).

## The left menu: an icon on every row

Every row of a left menu starts with a 16px stroke icon, then the label (PATTERNS.md, *Navigation icons*). In the
records frame the kit picks it with `navIcon({ icon, entity, kind, submittable, to })` (Ferrum-Frontend:
`packages/kit/src/navIconName.ts`), in this order:

1. the icon the row declares: a Lucide name in kebab-case on the entity (`icon:` on an entity definition,
   `crates/meta/src/types.rs`, `EntityDef.icon`), on a module (`ModuleDef.icon`; CRM's module says `icon: handshake`
   in `packages/crm/package.yaml`) or on a `shell.menu` row of the manifest (`docs/plugins.md`);
2. the concept dictionary (`CONCEPTS` in `navIconName.ts`), keyed on the words of the entity's stable name;
3. the row's kind (report, page, settings, document, submittable, else a list).

What you do:

- Give an entity of a new concept an `icon:` or add the concept to `CONCEPTS` in the kit. `navIconName.test.tsx`
  requires every entity name in `navIconName.entities.json` (regenerated by Ferrum-Frontend's
  `python3 scripts/nav-icon-entities.py` from the app repositories) to resolve through a concept word.
- Within one app's menu no icon on more than three rows unless the rows are the same kind of thing (the test's
  *repetition rule*; a declared exception goes in `EXCEPTIONS` with its reason).
- An app that draws a left menu of its own uses `NavIcon`, `navIconsReady` and `navIcon` from the kit; never import
  the drawing table (`NavIconTable.tsx`) statically. Mail's folders, Drive's places and Chat's channels keep their own
  marks.
- The launcher icon of a `shell.app` entry must be one the bar draws: `scripts/check_apps.py` fails "the shell draws
  no `<icon>` icon" against the `ICONS` table in `server/shell/runtime.js`.

## Phones

The contract asks that pages fit 375px (`docs/app-contract.md`, *Reviewed*); the kit's gallery and the release
candidates were looked at 390 and 320 wide. What you get and what you owe:

- `main` has 16px at the sides on phones; the page head's action row goes under the title below 720px; queue grids
  fall to one column at 390; `PageSplit` is one column under 900px.
- A coarse pointer makes every pressable thing 44px: buttons, tabs, chips, menu and select items, checkboxes and
  switches by a transparent hit area (PATTERNS.md, *Work home*, "Touch"). Do not override `--control-h`.
- Phones keep toasts at the top, under the bar, whatever a page asks (PATTERNS.md, *Where toasts sit*).
- Rows that scroll sideways (`.tabs`, `.segmented`, `.scroll-cue`, `.table-wrap`) fade the edge with more beyond; add
  `scroll-cue` to a new scrolling row and keep what is in it focusable (PATTERNS.md, *Scrolling rows*).
- Never block zoom: no `user-scalable=no`, no `maximum-scale=1` (the lint's `zoom-lock`).
- Inputs are 16px on touch so iOS does not zoom (Ferrum-Frontend: `README.md`, *Phones*).

## Dark mode, contrast and density

Every colour is defined once with `light-dark()`; the bootstrap's `theme.appearance` (`light`, `dark`, `system`) and
`lib/theme.ts` set `data-theme`, `data-contrast` and `data-density` on `<html>`. An app that uses tokens follows all
three without code. On public and portal pages a workspace's Brand sets the accent and typeface: its colour is
darkened in light mode and lightened in dark mode, step by step, until it reads at 4.5:1 on the page and surface
colours, and the colours shown are on the Brand record (`docs/builder/decisions.md` BD-79). The workspace's own pages
keep the suite's accent. What breaks it: a literal colour, an image with text on a white background, a chart
with colours of its own. The contract's review says "the colours hold in dark mode": look at every page in both
(`docs/app-contract.md`, *Reviewed*).

## Live updates

The bar keeps one live channel per browser (`docs/plan/apps-ui.md` §12.4). It carries ids, never data: refetch
through the API.

```tsx
import { useLiveEntity, useLivePush } from "@ferrum/kit";
useLiveEntity("Deal", (e) => { if (e.id === id) void q.refetch(); });   // e: {event, entity, id, actor}
useLivePush("drive-presence", (data) => (data === null ? refetchAll() : apply(data)));   // null after a break
```

`useLivePush` calls you with `null` when the channel comes back from a break: refetch then, since you may have missed
changes (Ferrum-Frontend: `packages/kit/src/live.ts`). Pass a stable handler (`useCallback`): the hooks subscribe
again when it changes. Where no shell runs, `live.subscribe` does nothing.

## Working offline

Only some apps work offline (Point of Sale, Field Service, Logistics, Docs). What a device may keep follows one policy
(`docs/offline.md`); the client gives the names:

- `cacheName(env, app, name)` for cached reads (null when nothing may be kept now), `pendingName(env, app, name)` for
  work not sent yet, `sweep()` at start to forget what the lease no longer covers, `renewOfflineLease()` for a page
  that stays open for days; `offlineEnv()` gives the `env` (Ferrum-Frontend: `packages/client/src/offline.ts`).
- Guests and portal people keep nothing. A page that cannot store anything works online only and says so.
- Send work kept offline with an idempotency key so a repeat is done once, and as what it does to the record as it
  stands then, not a whole copy that would undo someone else's change. Field Service's `ui/src/offline.ts` is the
  example to copy; its header says exactly this.
- `StatePanel kind="offline"` claims work is kept only when your code keeps it; `SaveStatus state="local"` says "kept
  on this device", never "saved".
- Not in the kit yet: an online/offline hook (`useOnline` lives in a module that acts on load; EVIDENCE.md,
  *Backlog*). Apps infer offline from a status of 0, as `failureKind` does.

## Checks before an interface ships

Run in `ui/`:

```sh
node ../../ferrum/web/packages/build/cli.mjs typecheck
node ../../ferrum/web/packages/build/cli.mjs test
node ../../ferrum/web/packages/build/cli.mjs build      # then commit ui/dist
node ../../ferrum/web/packages/build/cli.mjs check
```

`check` fails, with the message in brackets, when:

- `dist/` is missing or was built from other sources ("dist/ was built from other sources … run the build in ui/ and
  commit dist/"): the crate embeds `dist/`, so a source change without a build would ship stale files;
- the built CSS lacks the kit's base;
- a catalog in `src/locales/*.json` or `src/locales/names/*.json` holds a word the sources no longer ask for, an empty
  translation, or other `{places}` than the English;
- the entry JavaScript is over 200 KB compressed or the CSS over 50 KB ("over the size budget");
- the UI lint finds anything: there is no baseline to adopt, so every finding fails, and an exception is explained at
  the line.

The lint's rules (`packages/build/uilint.mjs`): `raw-color`, `small-text`, `arbitrary-text-size`, `blank-fallback`,
`zoom-lock`, `forked-primitive` (apps only), `allow-reason` (an allow comment without a reason) and `browser-zone` (Ferrum-Frontend's
own sources only, not an app's). It reads lines,
not a syntax tree: it finds what drifts silently, nothing more.

`scripts/app_ci.sh <app checkout>` in the platform runs `check`, `typecheck` and `test` in a copy of the app beside the
Frontend checkout, after the Rust steps ([15-testing-and-ci.md](15-testing-and-ci.md)). `scripts/check_apps.py` fails
an app whose `ui/dist` has no `index.html` and `.source`, or whose launcher icon the bar cannot draw.

## The acceptance scenarios

Ferrum-Frontend `docs/design/acceptance.json` holds 58 scenarios for the interface of the whole ecosystem (UX-001 to
UX-058), each with its exercise, expected result and the evidence it needs: the exact artifacts, the role, fixture,
viewport, theme and locale, and the result. All 58 read `not_run`. A scenario changes state only with retained
evidence, and a pass on one app says nothing about another (`ACCEPTANCE.md`). If your app is named in one
(`app:drive`, `app:comms/mail`, `app:erp`, …), that scenario is the walk to do before a release that changes it, and
its evidence is what you record.

What has been done instead is listed by evidence class in EVIDENCE.md: source review, component tests, real-browser
captures (two release-candidate sets of about 1,100 pictures each, light, dark, 390 wide, right to left, compact and
print), and no human design review, no assistive-technology session, no user study.

## Mistakes people make

| Mistake | What happens | Instead |
|---|---|---|
| `bg-[#2563eb]`, `color: #333` | lint `raw-color`; wrong in dark mode | a token |
| `text-[11px]` | lint `small-text` | `text-xs` |
| `<Suspense fallback={null}>` | lint `blank-fallback`; the page jumps | a skeleton of the content's shape |
| `import * as Dialog from "@radix-ui/react-dialog"` | lint `forked-primitive` | `Dialog` from the kit, or the primitive |
| `import { x } from "@/lib/api"` | the build fails | `@ferrum/client` |
| forgetting `@import` of the kit's `index.css` | `check` fails | the four lines above |
| building and not committing `dist/` | `check` and `scripts/check_apps.py` fail | build, then commit `ui/dist` with the sources |
| tone from `label.includes("Overdue")` | wrong colour once translated | `StatusBadge value={status_id}` |
| `"/crm/deals"` in a link | breaks when the route is remapped | `routeBase("crm")`, or the router's `basename` |
| a page-sized spinner for a list | loses the person's place | `TableSkeleton` |

## Checked by

- `packages/build/cli.mjs check` in Ferrum-Frontend: sources digest, base CSS, catalogs, budgets, UI lint.
- `packages/build/uilint.mjs` and its test `uilint.test.ts`; `budgets.test.ts`, `basecss.test.ts`, `words.test.ts`.
- `src/tokens.test.ts` (contrast of every token pair) and `packages/kit/src/navIconName.test.tsx` (menu icons) in
  Ferrum-Frontend: they hold the kit, not your app.
- `scripts/check_apps.py` (built `ui/dist`, launcher icon) and `scripts/app_ci.sh` (the interface step) here.
- Reviewed, not checked: the home's states, record pages through the platform's, 375px, dark mode, a walk on a
  running server (`docs/app-contract.md`, *Reviewed*).

## See also

- [18-accessibility.md](18-accessibility.md), [19-languages-and-formats.md](19-languages-and-formats.md),
  [20-writing-for-people.md](20-writing-for-people.md)
- [02-your-first-app.md](02-your-first-app.md) for a new app's first `ui/`; [03-the-manifest.md](03-the-manifest.md)
  for `ui` and `contributes`
- `docs/plan/apps-ui.md` (§4 rules, §10 the app contract, §12 platform services), `docs/plugins.md` (*Interfaces of
  their own*), `docs/offline.md`, `docs/app-contract.md`
- Ferrum-Frontend: `docs/design/README.md`, `FOUNDATIONS.md`, `PATTERNS.md`, `MIGRATION.md`, `EVIDENCE.md`,
  `ACCEPTANCE.md`, `acceptance.json`
