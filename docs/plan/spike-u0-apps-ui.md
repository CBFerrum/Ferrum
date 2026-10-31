# Spike U0: apps with interfaces of their own

30 October 2026. Phase U0 of [apps-ui.md](apps-ui.md) §17: two tiny apps with their own interfaces
under one shell on one host, to find out before building the rest whether the browser behaves as
the plan assumes. The apps are in [spikes/u0](spikes/u0); the checks run them in a browser with
Playwright against a development server.

The spike was built as the real U1 code rather than throwaway code: the kernel's sessions, hosts
and interface releases (`crates/kernel/src/shell.rs`), the manifest's `ui` section and `shell.*`
points (`crates/apps/src/shell.rs`, `ui.rs`), the server's shell (`server/src/shell.rs`,
`controllers/shell.rs`) and the bar runtime (`server/shell/`).

## Results

Google Chrome 156 on macOS. Firefox and WebKit are still to run: Playwright's own builds of
them are not installed on the machine the spike ran on, and downloading them needs the owner's go.

| Item | Chrome | Notes |
|---|---|---|
| Cookie session (`HttpOnly`, `SameSite=Lax`, `__Host-` on https) | **go** | A browser page signing in gets the cookie; an API client gets a bearer token as before. The cookie's token is refused as a bearer token. |
| Requests that change things only from the site's own pages (Fetch Metadata, Origin fallback) | **go** | `cross-site` and `same-site` POSTs with the cookie: 403. No `Sec-Fetch-Site` and a foreign `Origin`: 403. |
| Sockets with the cookie, `Origin` checked | **go** | `/ws/live` upgrades with the cookie from the page's origin; a revoked session closes it within a minute. |
| Entry pages from a stored release: bootstrap, theme, runtime, bar | **go** | `index.html` is split once when the release is made available; references become `/_ui/{app}/{digest}/…`. Deals was ready 29–141 ms after navigation (local server). |
| `<ferrum-bar>`: launcher, title, breadcrumbs, account menu | **go** | Shadow DOM: the apps' CSS does not reach it. |
| `ferrum.shell`: `navigate`, `title`, `ready`, `intent`, `leaveGuard`, `companion`, `live` | **go** | |
| One `pick` intent between the two apps | **go** | Deals asks; Notes' picker opens in a dialog frame and answers over a MessageChannel. |
| Live channel in a SharedWorker, one socket per browser | **go** | One change reached two tabs of the app over the shared socket. The per-tab fallback (no SharedWorker) was not exercised. |
| Companion window that outlives page loads | **go** | A call window kept running while the main tab loaded two other pages; every page's bar showed "Call · Return". Picture-in-Picture from the companion was not tried. |
| Back between apps from the back/forward cache | **go** | Deals came back from the cache (`pageshow.persisted`) with the shared worker and the session channel open. Playwright turns the cache off unless `--disable-back-forward-cache` is dropped. |
| Prerendering from the launcher | **partly** | Hovering an app adds a speculation rule (`'inline-speculation-rules'` in the policy). The measured click came before the prerender finished; switching was 49–68 ms from click to ready all the same. |
| Cross-document view transitions | **not measured** | `@view-transition { navigation: auto }` is in the theme. |
| Worker that removes itself where no app lives | **go** | `/{route}/sw.js` answers with it, scoped by `Service-Worker-Allowed`; it deletes only its route's caches. |
| An upgrade while a tab is open | **go** | The old release's files still load; the tab is told a new version runs and offers to reload; after the reload, the new release. |
| A person without an app's role | **go** | Not in the launcher, a 403 page at its address, its intents not offered; with the role, all three. |
| Suite and per-app installable web apps (nested manifest scopes) | **go** (Chrome) | Built in U5 (§12.5): Chrome finds no installability errors for the workspace (`id: /`) or for Point of Sale on its own (`id: /pos/`, nested in it). Edge and Safari were not tried. |
| Core worker for push, without a fetch handler | **go** | Built in U5 (§12.5): registered when someone turns notifications on; Chrome showed a message the server pushed through it. |
| `{workspace}.localhost` | **go** | Built in U6: each workspace at `{slug}.{base}`, `{slug}.localhost` in development; checked in Chrome, Firefox and WebKit. |

## What changed in the plan's design

* **Releases shared by versions.** A version that changes only its manifest has the same interface
  files, so the same release digest, as the one before. Releases and the versions that run them are
  now two tables (`ui_release`, `ui_release_version`).
* **Leave guards and the back/forward cache.** Firefox keeps pages with a `beforeunload` listener
  out of its cache, so the runtime listens only while an app has a guard registered.
* **Live topics.** Besides entity names, pages subscribe to `notification` and `push` (or
  `push:{kind}`); the worker never sends those to the server and swallows subscription replies.
* **Meta apps without packages.** An app of metadata may bring only an interface (over other apps'
  records): it needs a package or a `ui` section.
* **`ready` as a performance mark.** `ferrum:ready` can be read after the fact by tests and by
  real-user timing, unlike an event.
* **`/` stays a temporary redirect** to today's web app until the shell app takes it over, so no
  browser keeps a permanent redirect to `/app/`.

## How to run it

On a development server with unsigned uploads (the default in development):

    cargo run -q -p ferrum-apps --example pack -- docs/plan/spikes/u0/deals target/u0-deals.fapp
    cargo run -q -p ferrum-apps --example pack -- docs/plan/spikes/u0/notes target/u0-notes.fapp

Upload both (Apps, or `POST /api/admin/apps/upload`), install them, and open `/deals/` on the
server's own address.
