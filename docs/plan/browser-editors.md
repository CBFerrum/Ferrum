# Browser editors in Drive: GridCraft, WordCraft and PdfCraft (Phase 4a)

Plan, 30 October 2026. It answers the request to make DeckCraft, GridCraft, WordCraft and PdfCraft
workspace plugins. It builds on [platform.md](platform.md) §3 (plugin platform v2, built), §4 (Office
over WOPI, built) and spike S3 ([spike-s3-artcraft.md](spike-s3-artcraft.md)).

**Status, 30 October 2026: built** (Drive 0.4.0 with the add-on, Ferrum `deploy/editors` and the
web app's editor page), with its fidelity note. DeckCraft and the upstream pull requests (§8) are
still to come. The sections below describe what was built; where it differs from the first draft of this
plan, they say so.

## 1. The request, and where each part stands

| Asked for | Status |
|---|---|
| The four apps as a workspace plugin | This plan: the add-on **Browser editors** (`drive-editors`) under Drive. GridCraft (spreadsheets), WordCraft (documents) and PdfCraft (PDFs) now; DeckCraft (presentations) once it has the host bridge the others have (§8). |
| Plugins with their own dashboards | Built: pages and widgets of their own (`ui.page`, `ui.widget`), checked in three browser engines by spike S4. Dashboards of Studio charts seeded by a package (Tier 0) work through package seeds, but no app ships one yet (platform.md §6, Phase 1 remainder). |
| Plugins with their own endpoints | Built: `/api/ext/{plugin}/…`, public `/api/public/{tenant}/{plugin}/…`, sockets. Listing them in the API docs and as MCP tools is planned (platform.md §3.5). |
| Custom UI | Built: sandboxed screens that talk to the app through the `@ferrum/ui` bridge; large first-party screens in the web app. |
| Nested plugins in the one registry | Built: `parent` plus `requires`, a checked lifecycle, add-ons grouped under their parent on the Apps page and in the store. No real app uses it yet; `drive-editors` is the first. |

### Decisions
1. The browser editors are an optional add-on. The online editor (Collabora) stays the default: it
   co-edits, keeps more of a file and works with screen readers.
2. The add-on is called Browser editors (`drive-editors`). The ArtCraft name and logos appear only
   inside the editors; Ferrum names an editor by its product name in plain text ("Spreadsheet
   (GridCraft)").
3. The editors are the official, unmodified web builds, served by an editor server the operator runs
   on an origin of its own.
4. No fork: what the editors lack (DeckCraft's bridge, a Save button, read-only) is proposed upstream.

## 2. What the plan rests on

Checked on 30 October 2026 in the four repositories (github.com/storytold) and on getartcraft.com.

* **There is no hosted web version to wrap.** getartcraft.com offers desktop downloads and
  command-line tools. The web builds are assets of each GitHub release: GridCraft 0.13.0 (7 MB
  zipped), WordCraft 0.12.0 (17 MB), PdfCraft 0.15.0 (16 MB), DeckCraft 0.12.0 (10 MB). Framing a
  vendor's site would also send workspace files to it, so Ferrum serves the builds itself.
* **They run entirely in the browser**: Rust compiled to WebAssembly, drawn with egui on WebGL. The
  server only hands out files. A document never leaves the browser except through Drive.
* **Three of the four have a host bridge.** With `?host=parent`, GridCraft, WordCraft and PdfCraft
  exchange messages with the page that frames them:

  | Message | Direction | Content |
  |---|---|---|
  | `<app>:ready` | app → host | `{version}`: send a file now |
  | `<app>:open` | host → app | `{name, bytes}` |
  | `<app>:save` | app → host | `{name, bytes}` for every file the app would otherwise download: Save, Save As, exports |
  | `<app>:dirty` | app → host | `{dirty}` |
  | `<app>:failed` | app → host | `{error}`, for example no WebGL |

  * The bridge answers only a parent on its own origin.
  * `?chrome=none` hides the title bar and the File tab.
  * GridCraft takes `?author=` to sign comments.
  * There is no message for the host to ask for a save, and no read-only mode.
  * DeckCraft has no bridge, on `main` either.
* **They need a real origin.** In S3, GridCraft never started in an opaque-origin sandbox, which is
  how Ferrum runs plugin screens.
* **The builds cache well.** Trunk builds them with content-hashed file names and relative addresses
  (`filehash = true`, `public_url = "./"`), so only `index.html` has to be revalidated.
* **Formats:**
  * GridCraft: XLSX (styles, formulas, tables, conditional formats, charts and pivots
    round-trip), CSV, TSV.
  * WordCraft: DOCX, ODT, RTF, HTML, Markdown, TeX and text; opens DOC; exports PDF.
  * PdfCraft: PDF (read, organise, combine, split, secure, comment).
  * DeckCraft: PPTX, in alpha.
* **Licences and brand.**
  * The code is MIT or Apache-2.0, compatible with Ferrum's AGPL-3.0.
  * The brand terms (`docs/brand/LICENSE-brand.txt` in each repository) allow the ArtCraft name and
    logos only within each app and its official builds.
  * They must never appear in the name, icon, store listing, advertising or marketing of anything
    else.
  * A modified build must remove them. A fork kept only to send changes upstream may keep them.

## 3. Shape

```
Drive (today: files, versions, sharing, editing over WOPI)
├── drive.editor        new extension point: the editors offered for file types
├── online editor       today: Collabora or ONLYOFFICE over WOPI, set by the operator
└── drive-editors       new add-on "Browser editors": contributes GridCraft, WordCraft, PdfCraft
                        (a manifest only, no code)

Editor server           new, run by the operator like Collabora: ghcr.io/cbferrum/ferrum-editors
                        (built from Ferrum's deploy/editors)
├── /relay.html         Ferrum's relay, about a hundred lines
└── /gridcraft/ /wordcraft/ /pdfcraft/      the official builds, unmodified
```

**An add-on.**
* A workspace turns the editors on by installing it and off by removing it; files stay in Drive.
* New editors arrive as contributions, without a Drive release.

**An extension point now.**
* Drive has to choose among several editors per file type anyway: WOPI and three browser editors
  now, DeckCraft scheduled. So platform rule 5 holds.
* Its contract is data:

```yaml
# Drive's app.yaml (0.4.0)
extension_points:
  drive.editor:
    description: An editor for files of given types, run in the browser from the editor server
    schema:
      type: object
      required: [title, kind, program, types]
      additionalProperties: false
      properties:
        title: {type: string, maxLength: 60}
        kind: {enum: [browser]}
        program: {type: string, maxLength: 40}
        types: {type: array, maxItems: 20, items: {type: string, maxLength: 10}}
        max_mb: {type: integer}

# addons/editors/app.yaml in ferrum-app-drive (0.1.0)
id: drive-editors
name: Browser editors
parent: drive
requires: { drive: ">=0.4, <0.5" }
runtime: native
contributes:
  drive.editor:
    - { id: sheets, title: Spreadsheet (GridCraft), kind: browser, program: gridcraft, types: [xlsx, csv, tsv], max_mb: 50 }
    - { id: documents, title: Document (WordCraft), kind: browser, program: wordcraft, types: [docx, odt, rtf], max_mb: 50 }
    - { id: pdf, title: PDF (PdfCraft), kind: browser, program: pdfcraft, types: [pdf], max_mb: 100 }
```

How the built version differs from this plan's first draft:
* **The schema.** It uses only the keywords the platform's schema subset takes (no `pattern`,
  no `minimum`), and leaves out `id`, which the platform checks itself.
* **`program`, not `app`.** Each listed contribution already carries the contributing `app`.
* **`kind: browser`.**
* **The add-on ships with Drive.** It is a manifest compiled into the distribution, as native
  apps are, so it needs no store. A workspace installs it from its Apps page.

WOPI stays built into Drive and set by the operator. Rewriting working code into a contribution
would gain nothing.

**An editor server.**
* The bridge needs a parent on the app's own origin, and the apps need a real origin.
* That origin must never be Ferrum's, or the editors would reach its session.
* So the builds and a small relay live on an origin of their own.
* The server holds no data and no secrets: it serves static files.

**A relay.**
* It is the only Ferrum code on the editor origin.
* It frames the app on the same origin and passes messages between it and Drive's editor page.
* Drive speaks one neutral protocol, and the relay adapts it per app. An editor with a different
  bridge needs an adapter in the relay, not Drive code.

## 4. Opening and saving

1. **Open.**
   * `GET /api/app/drive/editors` says which kinds the server runs (`{online, browser}`).
   * "Open" uses the online editor when the server runs one and it opens the type; otherwise the
     first browser editor for it. A per-workspace default was left out: "Open with" covers the
     choice.
   * "Open with" lists every editor for the type; one that can't take the file says why.
2. **Launch.** The web app calls `POST /api/app/drive/editor/{file}` with `editor: sheets`.
   * Drive checks that the add-on is enabled and contributes the type.
   * Editing needs Edit rights, no check-out by someone else, and no other editor session holding
     the file's lock. That is the existing 30-minute WOPI lock, now shared by both kinds of editor.
   * Drive takes the lock and returns
     `{kind: bridge, url: <editor server>/relay.html, app, lock, version, name, action, held_by}`.
3. **Frame.** The editor page frames the relay with
   `sandbox="allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox"` and
   `allow="clipboard-read; clipboard-write; fullscreen"`.
   * Once the relay loads, the page posts `connect` with a `MessageChannel` port to the editor
     server's origin.
   * Every later message goes over that port.
4. **Start.** The relay accepts `connect` only from the origin in its `FERRUM_PUBLIC_URL`.
   * It checks that this server has the app.
   * It frames `/gridcraft/index.html?host=parent&chrome=none&author=…` on its own origin.
   * It waits up to 20 seconds for `gridcraft:ready`.
5. **Load.** The page downloads the file (`GET /api/app/drive/files/{file}/content`) and sends
   `open {name, bytes}`. The relay passes it on as `gridcraft:open`. Bytes are transferred, not
   copied.
6. **Edit.**
   * `dirty` drives the unsaved-changes mark and the guards against leaving.
   * The page refreshes the lock every 10 minutes.
7. **Save.** Ctrl/⌘+S or the app's own Save sends `gridcraft:save {name, bytes}`.
   * The page posts it to `POST /api/app/drive/files/{file}/versions?lock=…&base=<version>`, with
     the comment "Saved in GridCraft 0.13.0".
   * A save under another extension (Export as PDF, a DOC saved as DOCX) becomes a new file beside
     the original, under a unique name.
8. **Close.**
   * The page releases the lock, also when the page is hidden, with a keep-alive request.
   * A lock left behind lapses after 30 minutes.

## 5. Rules for the hard cases

| Case | What happens |
|---|---|
| A new, empty file (Drive's blank document or sheet) | Nothing is sent with `open`; the app starts with an empty document, as host mode does, and the first save fills the file. |
| The user may only view; someone else has checked the file out; another editor session holds its lock | It opens read-only and says why. Saving offers "Save a copy", which saves a new file where the user may write. |
| The same user opens it in a second tab | Read-only, with "Edit here instead", which takes the lock over. The first tab's next save gets a conflict. |
| The file changed since it was opened (`base` is not the current version) | A conflict: "Save a copy", or "Replace" (the other change stays in the history). |
| The lock lapsed, say while a laptop slept | The next save takes it again if nobody else holds it and the version is unchanged; otherwise the conflict above. |
| Collabora has the file open | The shared lock lets one session edit; the other opens read-only. |
| Someone uploads a new version in Drive while an editor session holds the lock | Refused (409, "being edited by …"). Today such an upload becomes a non-current version as soon as the online editor saves again; this closes that gap for both kinds. |
| The file is larger than the editor's `max_mb` | "Open with" shows the editor disabled, with the limit. Download and the online editor remain. |
| A save larger than the server's file limit (`FERRUM_DRIVE_MAX_FILE_MB`) | Refused with the limit. The changes stay in the editor until it is closed. |
| The editor doesn't start (no WebGL, blocked, 20 seconds) | The page says why and offers the type's other editors and Download. |
| The app is missing from the editor server (an older image) | The relay says so at once, and Drive logs it for the operator. |
| No editor server (`FERRUM_EDITORS_URL` empty) | Browser editors are hidden, and the Apps page marks the add-on as needing the editor server. |
| Leaving with unsaved changes | The browser's leave guard and the app's own ask first. |
| Phones and narrow screens | The editors draw on a canvas meant for mouse and keyboard. On narrow screens "Open" views the file (PDFs in the browser's viewer); the editors stay in "Open with". |
| The add-on is removed while files are open | Open sessions finish, and their saves still check rights and the lock. New opens are refused. |
| Types not contributed (macro workbooks, PPTX, encrypted files) | The online editor or Download. |

## 6. Security

**Two origins.**
* The editors never run on Ferrum's origin.
* So they can't read its session, which is kept in the page's storage, and they can't call its API.
  Ferrum uses no sign-in cookies, so a port of the same host is safe too.

**Headers on the editor server.**
* For the editors: `default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'
  'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self';
  worker-src 'self' blob:; form-action 'none'; base-uri 'none'; frame-ancestors 'self'`.
* Each editor's page also names the SHA-256 of Trunk's one inline loader script in `script-src`.
  The image build computes these sums, so no other inline script runs.
* For the relay: the same, with `frame-ancestors` set to `FERRUM_PUBLIC_URL`.
* `frame-ancestors` is checked against every ancestor, so the editors' pages allow the web app's
  origins (`FERRUM_PUBLIC_URL`) as well as `'self'`. Without that they would not load under the
  relay.
* Because of `connect-src 'self'`, an editor sends no requests elsewhere: no telemetry, no uploads.
  A page can still navigate itself with data in the address, which browsers offer no policy
  against. The defence there is the builds themselves: official, pinned, checked against their
  sums, and their source is open.
* Also `nosniff`, `Referrer-Policy: no-referrer`, and a `Permissions-Policy` that turns off the
  camera, microphone and location.

**The web app's policy.** It names the editor server in `frame-src`, from `FERRUM_EDITORS_URL` in
the web container, as `FERRUM_OFFICE_ORIGIN` does for Collabora.

**The frame.**
* Its sandbox gives the relay and the app their own origin, scripts and popups (help links open in a
  new tab), but never top-level navigation.
* Nested frames inherit these limits.

**Messages.**
* The relay accepts `connect` only from Ferrum's origin, and app messages only from its own app
  frame on its own origin.
* The editor page listens only to the port it created.
* Both sides check the shape and size of every message.

**The builds.**
* They are fetched from the GitHub release when the image is built and checked against pinned
  SHA-256 sums; the build fails on a mismatch.
* The image is published only after the browser tests pass.
* Upgrades are a reviewed change to the pins, never automatic.

**The user's name.** It reaches GridCraft in its address (`?author=`). The relay builds that address
in the browser, and the editor server's access log leaves query strings out.

**Rights stay Drive's.** Every open, save and lock goes through Drive's API with the user's own
rights. The editor server is trusted with nothing.

## 7. Deployment and operations

**The image.** `ghcr.io/cbferrum/ferrum-editors`, built from Ferrum's `deploy/editors/`. It lives
there, not in Drive's repository, so Ferrum's CI builds, tests and publishes it beside the server,
and the cluster test can deploy it.
* Unprivileged nginx with the three builds (about 40 MB compressed), precompressed `.wasm` and `.js`,
  and the relay.
* Non-root, a read-only root filesystem, no capabilities.
* Its one setting is `FERRUM_PUBLIC_URL`: whose pages may frame the relay.
  * The server refuses to start without it, or with anything but a plain address.
  * It writes its configuration to `/tmp`, the only place it may write.

**Ferrum's side.** `FERRUM_EDITORS_URL`, the editor server as browsers reach it, on both the server
and the web container.

**A single host.** `deploy/editors.yml` runs the editor server on a port of its own, for example
8090. Another port is another origin, so no DNS work is needed.

**Kubernetes.** `deploy/k8s/editors/`, an optional component:
* a Deployment of two replicas on different nodes, a Service and a PDB;
* an HTTPRoute for `editors.<domain>`, with its certificate;
* a NetworkPolicy that admits only the gateway;
* no service account token.

**Caching.** `index.html` and `relay.html` are revalidated. Everything else is content-hashed and
cached for a year, so a CDN can sit in front.

**Health.** The editor server answers `/healthz`, and Drive's settings page checks from the browser
that it is reachable.

**Upgrades.** Deploy a new image tag. Open editors keep the version they loaded, and the server
holds no state, so rolling updates go unnoticed.

**Cost.** Static files only, so two small replicas are enough. People download each editor once
(7–17 MB), then their browser caches it.

## 8. Upstream instead of a fork

Pull requests to the ArtCraft repositories:
1. **DeckCraft:** the host bridge the others have, modelled on WordCraft's
   `apps/wordcraft-web/src/host.rs`. Once released, DeckCraft joins with one contribution (`pptx`).
2. **All four:** a host-initiated save (`<app>:request-save`, answered with `<app>:save`), so Ferrum
   can show a Save button and save before closing.
3. **All four:** a read-only flag for viewers.
4. **WordCraft and PdfCraft:** honour `?chrome=none` as GridCraft does. They show their whole frame
   (File, Open, New, Share, community links) when a host owns the files
   ([browser-editors-fidelity.md](browser-editors-fidelity.md)).
5. **All four:** report a file they could not open (`<app>:failed`), rather than staying on their
   start page.

Until these land:
* the editor page says "Ctrl/⌘+S saves to Drive";
* read-only is enforced by refusing saves and offering "Save a copy";
* PPTX stays with the online editor.
* Only a file saved under the opened name is a new version of it. Anything else an editor hands
  back becomes a new file beside it.

If a change is declined, Ferrum lives with the gap. A modified build would have to drop the marks,
and it would become ours to maintain.

## 9. Work, in order

| # | Where | What | Done when |
|---|---|---|---|
| 1 | Ferrum `deploy/editors/` | The editor server image: Dockerfile, pinned versions and checksums, the relay, the headers; CI that builds, tests and publishes it | The relay round-trips a file with each app in Chromium, Firefox and WebKit; the headers are checked |
| 2 | ferrum-app-drive (Drive 0.4.0) | The `drive.editor` point; launching bridge editors; the shared lock (take, refresh, release, take over); the `base` check on new versions; uploads that respect editor locks | Unit and integration tests for every row of §5 the server decides |
| 3 | ferrum-app-drive `addons/editors/` | The add-on: manifest, contributions, store listing without the ArtCraft name or logos | It installs under Drive, and removing Drive while it is installed is refused |
| 4 | Ferrum-Frontend | Open and Open with; the editor page (frame, port, dirty, lock refresh, save, conflicts, read-only, failures); the default editor per type in Drive's settings; `frame-src` | Playwright tests for every row of §5 the browser decides, in the three engines |
| 5 | Ferrum `deploy/` and docs | `editors.yml`, the Kubernetes component, a "Browser editors" section in docs/operations.md | The Compose smoke test runs with the editor server; the kind test checks its headers |
| 6 | A fidelity note in docs/plan | Ten real files per type (styles, charts, merged cells, pivots, comments, tracked changes, images), opened and saved unchanged in each editor, compared with the original and with Collabora's result | Done: [browser-editors-fidelity.md](browser-editors-fidelity.md). 24 of 25 public files keep everything counted. Collabora was not compared (none ran here). |
| 7 | Upstream | The pull requests of §8 | Written up here; opening them on the ArtCraft repositories waits for the owner's go-ahead |

The phase is done when:
* in a workspace with the add-on, a sheet, a document and a PDF open from Drive, are edited and saved
  as versions in Chromium, Firefox and WebKit;
* a second user gets read-only;
* an online-editor session and a browser-editor session exclude each other;
* the deployment files and docs are in place, and the fidelity note is written.

About two weeks: items 1–3 in the first, 4–6 in the second.

## 10. How other web apps join Ferrum

The same question will come up for other web apps. Choose the first row that fits:

| The web app | How it joins | Ferrum code it needs |
|---|---|---|
| Runs in an opaque-origin sandbox and needs only Ferrum's API | A plugin screen (Tier 1): Ferrum serves its files | None |
| Edits files of given types and needs a real origin (GridCraft) | A `drive.editor` contribution, served by the editor server | A relay adapter if its bridge differs |
| Edits files and runs in a sandbox (a Markdown or diagram editor) | Later: `kind: screen` on `drive.editor`, opening and saving through the screen bridge | The new kind in Drive, once |
| Has its own server and an embedding protocol (Collabora, Stalwart) | A separate service the operator runs, fronted by an app or add-on (platform.md §3.6) | The protocol, once per kind |
| A hosted product with single sign-on | Later: a page that frames it, signed in through Ferrum's OpenID Connect | A page slot, once |
| Only an API (a payment gateway, the IRP) | A connector sub-plugin in WebAssembly | None |

## 11. Not building

* A fork or patched builds of the ArtCraft apps (brand terms, upkeep): changes go upstream.
* Anything that loads getartcraft.com: there is no hosted version, and files would leave the
  deployment.
* Co-editing in browser editors: one editor session at a time, by the lock. Co-editing is the online
  editor's job.
* The apps' server-side parts (PdfCraft's engine, the command-line tools): Gotenberg already
  converts and merges.
* A discovery file on the editor server: the contributions say what exists, and the relay reports
  what is missing.
* An origin per plugin, wildcard domains, a page that frames any address, offline editing, or format
  conversion in the browser.

## 12. Later, and what triggers it

| Item | When |
|---|---|
| DeckCraft for PPTX | Its host bridge is released upstream |
| `kind: screen` editors from any plugin | A plugin ships an editor that runs in a sandbox |
| An origin per third-party plugin (`<plugin>.apps.<domain>`, wildcard DNS and certificate) | A third-party plugin needs a real origin. The shared editor server is for reviewed builds only. |
| A page that frames a hosted product, signed in through Ferrum | Two such products are asked for |
| Diagrams (draw.io, Apache-2.0, whose embedding protocol works across origins) | Asked for |
| The editors' command-line tools for assistants (GridCraft's speaks MCP) | An assistant workflow needs server-side spreadsheets |

## 13. Risks

| Risk | Answer |
|---|---|
| Young software: DeckCraft is alpha, the others are at 0.x | An optional add-on; the online editor stays the default; the fidelity note decides defaults; versions are pinned and tested before every move |
| Accessibility: the editors draw on a canvas, with little for screen readers | The online editor stays the default and is always in "Open with" |
| One vendor whose terms may change | Released versions keep their licences; pinned builds; the brand terms are re-read at each upgrade |
| The bridge changes in a new version | The relay's browser tests run before a pin moves |
| Large files exhaust the browser's memory | `max_mb` per editor; the online editor for large files |
| A second origin is work for operators | A port on a single host; one more host name on the gateway |
| The brand terms | No ArtCraft name or logo in Ferrum's names, icons, listing or marketing; written permission is asked before anything more |

## Sources

* Repositories and releases: https://github.com/storytold/gridcraft, https://github.com/storytold/wordcraft,
  https://github.com/storytold/pdfcraft, https://github.com/storytold/deckcraft
* Host bridges: `apps/gridcraft-web/src/host.rs`, `apps/wordcraft-web/src/host.rs`,
  `apps/pdfcraft-web/src/main.rs`
* Brand terms: https://github.com/storytold/gridcraft/blob/main/docs/brand/LICENSE-brand.txt (the same
  terms in each repository, naming that app)
* Product pages: https://getartcraft.com/apps/gridcraft, https://getartcraft.com/apps/wordcraft,
  https://getartcraft.com/apps/pdfcraft, https://getartcraft.com/apps/deckcraft
