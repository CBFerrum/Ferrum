# Spike S3: ArtCraft editors in Office (go for GridCraft, without a fork)

30 October 2026. Spike S3 of [platform.md](platform.md) §6.

It ran the official web build of GridCraft 0.13.0 (`gridcraft-web-0.13.0.zip` from its GitHub
release, Apache-2.0) in headless Chrome, with a parent page standing in for Ferrum:
* `spikes/artcraft_site.py` makes the test workbook and the parent pages;
* `spikes/artcraft_embed.mjs` drives Chrome.

## Verdict

**Go for GridCraft as an optional Office editor, with no fork.** The plan assumed the ArtCraft
apps had no embedding API and needed a forked bridge (§4.3). Since October 2026 they have one:

* GridCraft's web build has a `postMessage` host bridge, `?host=parent`, made for apps such as
  Nextcloud that own the files:
  * `gridcraft:ready` and `gridcraft:failed` tell the parent whether the app started;
  * `gridcraft:open {name, bytes}` comes from the parent;
  * `gridcraft:save {name, bytes}` replaces every download (Save, Save As, Export as PDF);
  * `gridcraft:dirty {dirty}` reports unsaved changes.
* WordCraft has the same bridge (`apps/wordcraft-web/src/host.rs`), and PdfCraft handles
  `postMessage` in its web app. Code search found none in DeckCraft (0.12.0).
* `?chrome=none` hides the title bar and the File tab, so the parent owns which file is open.

## Checked

| What | Result |
|---|---|
| Start, same-origin parent | `gridcraft:ready` in about 0.2 s (local files) |
| Open | the parent sends a workbook (a sheet "Sales", values, `SUM(B2:B3)`). It shows correctly; GridCraft only notes that the minimal file has no styles part |
| Save | Ctrl+S in the editor sends `gridcraft:save` (`test.xlsx`, 4 162 bytes) to the parent |
| Round trip | the saved workbook keeps the sheet name, the values and the formula, and gains the parts the minimal file lacked |
| Opaque-origin sandbox (Ferrum's screens) | never starts: no `ready`, no `failed` |
| Size | GridCraft 20 MB of WebAssembly (7 MB zipped). Zipped web builds: WordCraft 17 MB, DeckCraft 10 MB, PdfCraft 16 MB |

## What changes in the plan (§4)

* **No fork for GridCraft and WordCraft.** Ship their official web builds, pinned and unmodified,
  as `office-artcraft`. Before relying on that, check their trademark policy for redistributing
  official builds; the plan only knew that forks must rebrand.
* **A separate editor origin, not a Tier 1 screen.**
  * The bridge talks only to a parent on its own origin, and GridCraft needs `allow-same-origin`.
  * So the editors, and a small Ferrum relay page beside them, live on an editor origin
    (`editors.<domain>`), the same pattern as Collabora's server.
  * Ferrum frames the relay. The relay frames the editor with `?host=parent&chrome=none` and passes
    bytes between `gridcraft:*` and Ferrum's screen bridge, which opens and saves through Drive.
  * The editors never share Ferrum's origin, so they cannot reach its session.
* **Still to check before Office ships them:**
  * fidelity on real files: styles, charts, merged cells and pivots, against Collabora on the
    same files;
  * DeckCraft's embedding;
  * how Save is offered when `chrome=none` hides the File tab. The shortcut works; a host-side
    Save button would need a message the bridge does not have yet.
* Collabora stays the default editor (co-editing, fidelity); ArtCraft is the light,
  browser-only option.
