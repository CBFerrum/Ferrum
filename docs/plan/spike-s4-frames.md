# Spike S4: apps' screens in Chromium, Firefox and WebKit (go)

30 October 2026. Spike S4 of [platform.md](platform.md) §6. It ran after Phase 1 shipped, against
what it built: the board example (ferrum-sdk `examples/board`: a page, a widget, a panel and a
settings screen) installed in a workspace with `ferrum-app dev --install`, the web app in front.
Engines: Playwright 1.64.1 with Chromium 156, Firefox 157 and WebKit (build 2370).

`spikes/screens_engines.mjs` repeats it:

    npm i playwright@1.64.1 && npx playwright install chromium firefox webkit
    WEB=http://localhost:5175 API=http://127.0.0.1:5150 TENANT=demo EMAIL=… PASSWORD=… node screens_engines.mjs

It needs the web app at `WEB`, its API at `API`, and the board app installed in the workspace.

## Verdict

Go, after one fix. All 13 checks now pass in all three engines. On the first run WebKit failed one:
a screen could not `fetch()` its own files. It refused them as outside `connect-src 'self'`.

* Chromium and Firefox match `'self'` in a sandboxed frame against the frame's URL.
* WebKit matches it, for fetches, against the frame's opaque origin, which nothing matches. Module
  scripts under `script-src 'self'` still loaded.
* So in Safari a screen could run but not load its own JSON or WebAssembly.
* The fix names the server's origins next to `'self'` in every directive of a screen's Content
  Security Policy:
  * the operator's `PUBLIC_URL`;
  * the origin the request arrived for (its forwarded or `Host` header).
  * Only plain `http(s)://host[:port]` values are taken, so a forged header cannot add directives.
  * Explicit host sources match the request's URL, whatever the frame's origin.

## Checked, in each engine

| Check | How |
|---|---|
| The page loads in its frame and gets its context | `#who` shows the signed-in user |
| A read through the bridge | the card list fills |
| A write through the bridge | a new card appears in the list |
| WebAssembly compiles in the frame (`'wasm-unsafe-eval'`) | a small module's `add(2, 3)` returns 5 |
| The screen fetches its own files (`connect-src`, CORS from an opaque origin) | `fetch("board.js")` succeeds |
| The API is out of reach without the bridge | a direct `fetch("/api/v1/BdCard")` gets no answer it can read |
| Storage is out of reach | `localStorage` throws |
| The parent page is out of reach | `parent.document` throws |
| The frame's origin is opaque | `self.origin` is `"null"` |
| The widget on Home | the card count shows |
| The panel on a record's form | the card's title shows, read through the bridge |
| Navigation from a screen | the panel's button opens the board page |
| A screen opened as a page of its own is refused | HTTP 403 |

## For operators

`PUBLIC_URL` must be the address people open, scheme and port included, as the deployment
manifests already set it. Without it, screens still work in Safari when the server sees the browser's
host, as behind the gateway or the web app's nginx. A development proxy that rewrites the host (Vite's
`changeOrigin`) needs `PUBLIC_URL` set to the web app's address.
