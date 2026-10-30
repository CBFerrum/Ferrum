// Ferrum's relay (docs/plan/browser-editors.md). Ferrum's web app frames relay.html?program=<name>;
// the relay frames that editor on this origin, where its host bridge answers, and passes files
// between the two. It holds no session and sends nothing anywhere but to these two frames.
//
// Ferrum → relay: `connect {author}` with a MessagePort, once; then over the port `open {name, bytes}`
// (bytes empty or absent: a new file, which the editor starts empty).
// relay → Ferrum, over the port: `ready {version}`, `dirty {dirty}`, `save {name, bytes}`,
// `failed {error}`, where error is `unknown`, `missing`, `timeout` or the editor's own reason.
// The editors speak `<program>:ready|open|save|dirty|failed` (?host=parent) to a parent on their origin.
"use strict";

const START_MS = 30000;
const MAX_BYTES = 1 << 30;
const program = new URLSearchParams(location.search).get("program") || "";
const allowed = new Set(
  String(self.FERRUM_ORIGINS || "")
    .split(/\s+/)
    .map((o) => {
      try {
        return new URL(o).origin;
      } catch {
        return null;
      }
    })
    .filter(Boolean),
);

let port = null;
let editor = null;
let ready = false;
let pending = null;
let timer = null;

function tell(message, transfer) {
  if (port) port.postMessage(message, transfer || []);
}

function fail(error) {
  clearTimeout(timer);
  tell({ type: "failed", error });
}

addEventListener("message", (e) => {
  if (editor && e.source === editor.contentWindow && e.origin === location.origin) {
    fromEditor(e.data);
    return;
  }
  // Ferrum: one connect, from the web app framing this page.
  const d = e.data;
  if (port || e.source !== parent || !allowed.has(e.origin) || !d || d.type !== "connect" || !(e.ports[0] instanceof MessagePort)) return;
  port = e.ports[0];
  port.onmessage = (m) => fromFerrum(m.data);
  start(typeof d.author === "string" ? d.author.slice(0, 100) : "");
});

async function start(author) {
  if (!/^[a-z][a-z0-9-]{0,39}$/.test(program)) return fail("unknown");
  try {
    const r = await fetch(`${program}/index.html`, { method: "HEAD", cache: "no-store" });
    if (!r.ok) return fail("missing");
  } catch {
    return fail("missing");
  }
  const q = new URLSearchParams({ host: "parent", chrome: "none" });
  if (author) q.set("author", author);
  editor = document.createElement("iframe");
  editor.title = "Editor";
  editor.allow = "clipboard-read; clipboard-write; fullscreen";
  editor.src = `${program}/index.html?${q}`;
  document.body.append(editor);
  timer = setTimeout(() => fail("timeout"), START_MS);
}

function fromFerrum(m) {
  if (!m || m.type !== "open" || typeof m.name !== "string" || m.name.length > 255) return;
  const bytes = m.bytes instanceof ArrayBuffer && m.bytes.byteLength > 0 && m.bytes.byteLength <= MAX_BYTES ? m.bytes : null;
  pending = bytes ? { name: m.name, bytes } : null;
  flush();
}

function flush() {
  if (!ready || !pending) return;
  const { name, bytes } = pending;
  pending = null;
  editor.contentWindow.postMessage({ type: `${program}:open`, name, bytes }, location.origin, [bytes]);
}

function fromEditor(m) {
  const prefix = `${program}:`;
  if (!m || typeof m.type !== "string" || !m.type.startsWith(prefix)) return;
  switch (m.type.slice(prefix.length)) {
    case "ready":
      clearTimeout(timer);
      ready = true;
      tell({ type: "ready", version: String(m.version || "").slice(0, 40) });
      flush();
      break;
    case "dirty":
      tell({ type: "dirty", dirty: !!m.dirty });
      break;
    case "save":
      if (typeof m.name === "string" && m.bytes instanceof ArrayBuffer && m.bytes.byteLength <= MAX_BYTES) {
        tell({ type: "save", name: m.name.slice(0, 255), bytes: m.bytes }, [m.bytes]);
      }
      break;
    case "failed":
      fail(String(m.error || "the editor did not start").slice(0, 300));
      break;
  }
}
