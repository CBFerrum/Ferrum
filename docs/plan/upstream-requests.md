# Browser editors: requests to the ArtCraft repositories

30 October 2026. The upstream part of [browser-editors.md](browser-editors.md) §8.

**Posted 30 October 2026**, as drafted below:
* storytold/deckcraft#1563 (https://github.com/storytold/deckcraft/issues/1563);
* storytold/wordcraft#4945 (https://github.com/storytold/wordcraft/issues/4945);
* storytold/gridcraft#2942 (https://github.com/storytold/gridcraft/issues/2942).

The asks already open upstream (below) were left as they are.

The maintainers are building a richer protocol for hosts that own the files (a "managed host").
It has a private message port for sandboxed frames, read-only sessions, an "opened" message and
save acknowledgements. Most of what Ferrum needs is already asked for there, so the plan is:
* follow and support those issues;
* post only the three requests nobody has made.

## Already asked for upstream

| Need (§8) | Issues |
|---|---|
| Read-only for viewers | gridcraft#2354 (host-controlled read-only session), wordcraft#2229 (lock and unlock editing from the host) |
| Know that a file opened, or did not | gridcraft#1984, pdfcraft#3516 (post `:opened` after a hub-supplied document loads) |
| PdfCraft's frame without external links | pdfcraft#2490 (an embedded mode that hides the Discord, GitHub and website links) |
| A save the host could not store is not marked done | pdfcraft#2167 |
| Editors in an opaque sandbox, without a second origin | gridcraft#2740, wordcraft#4591 (a capability `MessagePort` for opaque sandboxed frames) |

The last row would change Ferrum's design for the better, once released:
* the editors could run as ordinary plugin screens (Tier 1) in the opaque sandbox;
* operators would need no editor server origin;
* the relay would no longer be needed.

It is in browser-editors.md §12.

## New requests (as posted)

### storytold/deckcraft: "Web build: a host bridge (?host=parent) like GridCraft's and WordCraft's"

> GridCraft, WordCraft and PdfCraft's web builds have a `postMessage` bridge for pages that own the
> files (`?host=parent`): `:ready`, `:open {name, bytes}`, `:save {name, bytes}` in place of downloads,
> `:dirty {dirty}` and `:failed {error}`, heard only from a same-origin parent. DeckCraft 0.12.0's web
> build has none, so a host cannot open a presentation from its storage or receive the saved file.
>
> Could DeckCraft's web app get the same bridge, as in `apps/wordcraft-web/src/host.rs` (with
> `deckcraft:` messages and `?chrome=none`)? We run GridCraft, WordCraft and PdfCraft this way in an
> ERP's file storage (each save becomes a version) and would add presentations the day DeckCraft
> has it.

### storytold/wordcraft: "Web build: honour ?chrome=none in host mode, as GridCraft does"

> With `?host=parent&chrome=none`, GridCraft leaves out its title bar and File tab, so the host
> decides which file is open. WordCraft 0.12.0 still shows its whole frame in host mode: File (Open,
> New, Save As), AutoSave, Share and the community link. A person can then open another file
> inside the frame, and the host receives it under a name it did not open.
>
> Could WordCraft hide the title bar and the File tab when `chrome=none` is given (and accept
> `chrome=notitlebar` / `chrome=nofiletab` as GridCraft does)? Host mode would keep Save (Ctrl+S),
> which goes to the host.

### storytold/gridcraft, for all three bridges: "Host bridge: let the host ask for a save"

> In host mode, saving starts in the app (Ctrl+S, or the File tab, which `chrome=none` hides). The
> host cannot offer its own Save button, or save before it closes the frame.
>
> Could the bridge accept `gridcraft:request-save {}` from the parent, and answer as a Save would
> (`gridcraft:save {name, bytes}`)? If there is nothing to save, it could answer with `:dirty
> {dirty: false}` instead. With save acknowledgements (#2740's managed saves), the host could then
> confirm the store as well.

## Comments, if wanted

A short note of support, with Ferrum's use case, on gridcraft#2354, wordcraft#2229,
gridcraft#1984, pdfcraft#3516, pdfcraft#2490 and gridcraft#2740. They are optional; the issues
already say what is needed.
