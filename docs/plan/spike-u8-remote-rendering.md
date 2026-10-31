# Spike U8: remote rendering

**Question.** Can another app's small extension (a rating widget on a Helpdesk ticket, a field on a
CRM deal) look and behave like the host's own interface without its code running in the host's
page? This is §11.1's fifth way for apps to appear in each other, the one Shopify calls
remote-dom.

**Answer: yes, with little code. Build it when a third party asks for native-looking extensions.**
Until then, isolated frames (screens, and full interfaces since U8) cover third parties.

## What was built

`docs/plan/spikes/u8_remote/` is plain HTML and JavaScript with no build step. A test runs it in
Chrome: `spikes/u8_remote_check.mjs`.

* **The extension** (`extension.js`) runs in a frame with `sandbox="allow-scripts"`. Its origin is
  opaque: it has no DOM of the host's, no cookie, no storage. It describes its UI as a tree of
  the host's components, `{type, props, children}`, and turns functions into handles,
  `{"$fn": n}`. It sends the tree over a MessagePort whenever its state changes.
* **The host** (`host.js`) draws only components it knows: Stack, Row, Heading, Text, Badge,
  Button, TextField and Link. Each is built from props the host checks:
  * text goes in as `textContent` and is cut to a limit;
  * tones and kinds come from lists;
  * links are allowed only to the site's own paths.
  
  Unknown components are refused, as are trees over 500 nodes or 20 levels deep. An event calls
  the handle back; the extension changes its state and sends a new tree.

## Findings

* **Isolation holds.** From the extension: the parent's document is unreachable, `document.cookie`
  throws, `localStorage` throws, and `self.origin` is `null`. A `Script` node and a
  `javascript:` link are refused and logged; a link to `/helpdesk/t/42` is drawn.
* **Speed is not a concern.** Drawing the widget plus 100 rows takes 0.2 ms in Chrome; plus 480
  rows, 0.5 ms. A round trip (click, new state, new tree, redraw) is one postMessage each way.
  Redrawing the whole tree each time is fine at these sizes; keyed diffing (remote-dom's
  mutations) is for lists of thousands.
* **Module scripts need CORS from an opaque origin**, as with isolated interfaces. The spike's
  extension is a classic script. A real host serves extension files with
  `Access-Control-Allow-Origin: *` and `Cross-Origin-Resource-Policy: cross-origin`, as
  `/_isolated/` does.
* **The components are the contract.** What makes extensions look native is a small, versioned set
  of kit components with checked props. They are versioned like the shell API, so old
  extensions keep working when the kit changes. Accessibility comes from the host's
  components (labels, focus, keyboard), not from each extension.
* **What it cannot do:** anything outside the component set (canvas, custom layouts, rich text
  editing). Those stay with frames.

## If it is built

* `@ferrum/remote`, the extension side: `h()`, `render()`, handles, and a typed component set.
  The kit renders the host side. Protocol 1 sits next to screens' protocol 1.
* Extension points declare `remote: true` slots (a ticket's sidebar, a deal's tab). The store
  reviews which components an extension uses.
* The extension's code calls the API through the host, like screens, narrowed to its app's
  `access` or plugin permissions.
* Workers inside the sandboxed frame keep heavy extensions off the host's main thread. The frame
  can start them from blob URLs.
