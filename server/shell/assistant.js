// The assistant's page (server/src/assistant.rs, controllers/assistant.rs): the bar's side panel,
// or the whole page on a phone. Plain modules like the bar's runtime, drawn with the theme's
// tokens. It talks to /api/assistant and follows a turn's events as they stream; a turn goes on
// when the page goes away, and the next page of any app shows it again.

// Lucide icons (lucide.dev, ISC license, Copyright (c) Lucide Icons and Contributors).
const ICONS = {
  "arrow-up": '<path d="m5 12 7-7 7 7"/><path d="M12 19V5"/>',
  square: '<rect width="18" height="18" x="3" y="3" rx="2"/>',
  sparkles:
    '<path d="M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z"/><path d="M20 2v4"/><path d="M22 4h-4"/><circle cx="4" cy="20" r="2"/>',
  plus: '<path d="M5 12h14"/><path d="M12 5v14"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  "circle-alert": '<circle cx="12" cy="12" r="10"/><line x1="12" x2="12" y1="8" y2="12"/><line x1="12" x2="12.01" y1="16" y2="16"/>',
  "loader-circle": '<path d="M21 12a9 9 0 1 1-6.219-8.56"/>',
  wrench:
    '<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.106-3.105c.32-.322.863-.22.983.218a6 6 0 0 1-8.259 7.057l-7.91 7.91a1 1 0 0 1-2.999-3l7.91-7.91a6 6 0 0 1 7.057-8.259c.438.12.54.662.219.984z"/>',
  "chevron-down": '<path d="m6 9 6 6 6-6"/>',
  search: '<path d="m21 21-4.34-4.34"/><circle cx="11" cy="11" r="8"/>',
  "shield-check":
    '<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/>',
  "circle-x": '<circle cx="12" cy="12" r="10"/><path d="m15 9-6 6"/><path d="m9 9 6 6"/>',
  trash: '<path d="M10 11v6"/><path d="M14 11v6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M3 6h18"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>',
  history: '<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l4 2"/>',
};

const SVG_NS = "http://www.w3.org/2000/svg";
function icon(name, size = 16) {
  const svg = document.createElementNS(SVG_NS, "svg");
  for (const [k, v] of Object.entries({ width: size, height: size, viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": 2, "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" }))
    svg.setAttribute(k, String(v));
  svg.innerHTML = ICONS[name] ?? "";
  return svg;
}

const el = (tag, attrs = {}, ...children) => {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "text") e.textContent = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v === true ? "" : v);
  }
  e.append(...children.filter((c) => c !== null && c !== undefined && c !== false));
  return e;
};

const shell = () => globalThis.ferrum?.shell;
const inPanel = window.top !== window;
const THREAD_KEY = "ferrum.assistant.thread";
const remember = (id) => {
  try {
    if (id) sessionStorage.setItem(THREAD_KEY, id);
    else sessionStorage.removeItem(THREAD_KEY);
  } catch {}
};
const remembered = () => {
  try {
    return sessionStorage.getItem(THREAD_KEY);
  } catch {
    return null;
  }
};

// ───────────────────────── Markdown (escaped first) ─────────────────────────

const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

/** Inline Markdown on escaped text: code, links (http(s) and this site's paths only), bold, italics. */
function inline(s) {
  return s
    .split(/(`[^`\n]+`)/g)
    .map((part) => {
      if (/^`[^`\n]+`$/.test(part)) return `<code>${esc(part.slice(1, -1))}</code>`;
      return esc(part)
        .replace(/\[([^\]\n]+)\]\(([^)\s]+)\)/g, (_, label, url) => {
          const u = url.replace(/&amp;/g, "&");
          if (/^https?:\/\//i.test(u)) return `<a href="${esc(u)}" target="_blank" rel="noopener noreferrer">${label}</a>`;
          if (/^\/(?!\/)/.test(u)) return `<a href="${esc(u)}" data-here>${label}</a>`;
          return label;
        })
        .replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>")
        .replace(/(^|[^*\w])\*([^*\s][^*\n]*?)\*(?!\w)/g, "$1<em>$2</em>")
        .replace(/(^|[^_\w])_([^_\s][^_\n]*?)_(?!\w)/g, "$1<em>$2</em>");
    })
    .join("");
}

/** Markdown to HTML: paragraphs, headings, lists, tables, quotes and code blocks. */
function markdown(src) {
  const lines = String(src ?? "").replace(/\r\n/g, "\n").split("\n");
  const out = [];
  let i = 0;
  const isTableSep = (l) => /^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$/.test(l);
  const cells = (l) => l.trim().replace(/^\|/, "").replace(/\|$/, "").split("|").map((c) => c.trim());
  while (i < lines.length) {
    const line = lines[i];
    const fence = line.match(/^\s*```(\S*)/);
    if (fence) {
      const code = [];
      i++;
      while (i < lines.length && !/^\s*```/.test(lines[i])) code.push(lines[i++]);
      i++;
      out.push(`<pre><code>${esc(code.join("\n"))}</code></pre>`);
      continue;
    }
    if (!line.trim()) {
      i++;
      continue;
    }
    const h = line.match(/^(#{1,4})\s+(.*)$/);
    if (h) {
      const level = Math.min(6, h[1].length + 2);
      out.push(`<h${level}>${inline(h[2])}</h${level}>`);
      i++;
      continue;
    }
    if (line.includes("|") && i + 1 < lines.length && isTableSep(lines[i + 1])) {
      const head = cells(line);
      i += 2;
      const rows = [];
      while (i < lines.length && lines[i].includes("|") && lines[i].trim()) rows.push(cells(lines[i++]));
      out.push(
        `<div class="fa-table"><table><thead><tr>${head.map((c) => `<th>${inline(c)}</th>`).join("")}</tr></thead><tbody>${rows
          .map((r) => `<tr>${head.map((_, j) => `<td>${inline(r[j] ?? "")}</td>`).join("")}</tr>`)
          .join("")}</tbody></table></div>`,
      );
      continue;
    }
    const item = line.match(/^\s*([-*+]|\d+[.)])\s+(.*)$/);
    if (item) {
      const ordered = /\d/.test(item[1]);
      const items = [];
      while (i < lines.length) {
        const m = lines[i].match(/^\s*([-*+]|\d+[.)])\s+(.*)$/);
        if (m && /\d/.test(m[1]) === ordered) items.push(m[2]);
        else if (lines[i].trim() && /^\s{2,}\S/.test(lines[i]) && items.length) items[items.length - 1] += ` ${lines[i].trim()}`;
        else break;
        i++;
      }
      const tag = ordered ? "ol" : "ul";
      out.push(`<${tag}>${items.map((t) => `<li>${inline(t)}</li>`).join("")}</${tag}>`);
      continue;
    }
    if (/^\s*>/.test(line)) {
      const quote = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) quote.push(lines[i++].replace(/^\s*>\s?/, ""));
      out.push(`<blockquote>${inline(quote.join(" "))}</blockquote>`);
      continue;
    }
    const para = [];
    while (i < lines.length && lines[i].trim() && !/^\s*(```|#{1,4}\s|[-*+]\s|\d+[.)]\s|>)/.test(lines[i])) para.push(lines[i++]);
    if (!para.length) para.push(lines[i++]);
    out.push(`<p>${para.map(inline).join("<br>")}</p>`);
  }
  return out.join("");
}

function rich(text) {
  const div = el("div", { class: "fa-md" });
  div.innerHTML = markdown(text);
  return div;
}

// A link to a page of the suite opens there (the host page, from the side panel).
document.addEventListener("click", (e) => {
  const a = e.target.closest?.("a[data-here]");
  if (!a || e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
  e.preventDefault();
  const href = a.getAttribute("href");
  if (shell()?.navigate) shell().navigate(href);
  else location.assign(href);
});

// ───────────────────────── talking to the server ─────────────────────────

async function failure(res) {
  let message = `The server answered ${res.status}.`;
  try {
    const body = await res.json();
    message = body.message ?? body.error ?? message;
  } catch {}
  const err = new Error(message);
  err.status = res.status;
  return err;
}

async function api(method, path, body) {
  const res = await fetch(path, {
    method,
    credentials: "same-origin",
    headers: { accept: "application/json", ...(body === undefined ? {} : { "content-type": "application/json" }) },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (res.status === 401) {
    location.assign(`/login?next=${encodeURIComponent(location.pathname)}`);
    throw new Error("Signed out.");
  }
  if (!res.ok) throw await failure(res);
  return res.status === 204 ? null : res.json();
}

/** Reads a turn's server-sent events until it ends (or the page stops following). */
async function events(path, body, signal) {
  const res = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    credentials: "same-origin",
    headers: { accept: "text/event-stream", ...(body === undefined ? {} : { "content-type": "application/json" }) },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal,
  });
  if (!res.ok) throw await failure(res);
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    buf += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n");
    let end;
    while ((end = buf.indexOf("\n\n")) >= 0) {
      const raw = buf.slice(0, end);
      buf = buf.slice(end + 2);
      const data = raw
        .split("\n")
        .filter((l) => l.startsWith("data:"))
        .map((l) => l.slice(5).replace(/^ /, ""))
        .join("\n");
      if (!data) continue;
      let ev;
      try {
        ev = JSON.parse(data);
      } catch {
        continue;
      }
      onEvent(ev);
    }
  }
}

// ───────────────────────── state ─────────────────────────

let status = null; // {available, reason, admin, threads}
let thread = null; // {id, title, state, messages, pending, draft}
let following = null; // the event stream being read
let poll = 0;
const tools = new Map(); // tool call id → its row
let draftNode = null;
let pendingNode = null;
let menuOpen = false;

const log = el("div", { class: "fa-log", role: "log", "aria-live": "polite", "aria-relevant": "additions" });
const input = el("textarea", { class: "fa-input", rows: "1", placeholder: "Ask about your work…", "aria-label": "Message the assistant", maxlength: "20000" });
const sendBtn = el("button", { type: "submit", class: "fa-send", "aria-label": "Send", title: "Send" }, icon("arrow-up", 18));
const titleEl = el("h1", { class: "fa-title", text: "Assistant" });
const menu = el("div", { class: "fa-menu", role: "dialog", "aria-label": "Conversations", hidden: true });
const historyBtn = el("button", { type: "button", class: "fa-icon-btn", "aria-label": "Conversations", title: "Conversations", "aria-expanded": "false", onclick: () => toggleMenu() }, icon("history", 17));
const newBtn = el("button", { type: "button", class: "fa-icon-btn", "aria-label": "New conversation", title: "New conversation", onclick: () => fresh() }, icon("plus", 18));
const form = el(
  "form",
  { class: "fa-compose", onsubmit: (e) => (e.preventDefault(), running() ? stop() : send()) },
  el("div", { class: "fa-field" }, input, sendBtn),
  el("p", { class: "fa-hint", text: "The assistant can make mistakes. You approve every change it makes." }),
);

const running = () => thread?.state === "running";

function setBusy() {
  const busy = running();
  sendBtn.replaceChildren(icon(busy ? "square" : "arrow-up", busy ? 14 : 18));
  sendBtn.setAttribute("aria-label", busy ? "Stop" : "Send");
  sendBtn.title = busy ? "Stop" : "Send";
  sendBtn.classList.toggle("fa-stop", busy);
  input.disabled = thread?.state === "waiting";
  input.placeholder = thread?.state === "waiting" ? "Allow or refuse the change above first…" : "Ask about your work…";
}

// ───────────────────────── drawing ─────────────────────────

const TOOL_WORDS = {
  ferrum_overview: "Looked at the workspace",
  describe_entity: "Read how records are kept",
  search_records: "Searched records",
  get_record: "Opened a record",
  preview_record: "Previewed a record",
  run_report: "Ran a report",
  get_audit_trail: "Read a record's history",
  get_collaboration: "Read comments and activity",
  my_assignments: "Looked at your assignments",
  create_record: "Create a record",
  update_record: "Change a record",
  delete_record: "Delete a record",
  submit_record: "Submit a record",
  cancel_record: "Cancel a record",
  amend_record: "Amend a record",
  run_action: "Run an action",
  add_comment: "Comment",
  assign_record: "Assign a record",
  set_tags: "Tag a record",
  workflow_transition: "Move a record on",
  customize_entity: "Customize records",
};

function toolLabel(item) {
  const entity = item.input?.entity ? ` · ${item.input.entity}` : "";
  return `${TOOL_WORDS[item.name] ?? item.title ?? item.name.replace(/_/g, " ")}${entity}`;
}

function toolRow(item) {
  const statusIcon = el("span", { class: "fa-tool-status" }, icon("loader-circle", 14));
  const detail = el("pre", { class: "fa-tool-detail", hidden: true, text: JSON.stringify(item.input ?? {}, null, 2) });
  const row = el(
    "div",
    { class: "fa-tool", "data-state": "running" },
    el(
      "button",
      {
        type: "button",
        class: "fa-tool-head",
        "aria-expanded": "false",
        onclick: (e) => {
          detail.hidden = !detail.hidden;
          e.currentTarget.setAttribute("aria-expanded", String(!detail.hidden));
        },
      },
      icon(item.name === "search_records" ? "search" : "wrench", 14),
      el("span", { class: "fa-tool-name", text: toolLabel(item) }),
      statusIcon,
    ),
    detail,
  );
  row.finish = (error, text) => {
    row.dataset.state = error ? "error" : "done";
    statusIcon.replaceChildren(icon(error ? "circle-alert" : "check", 14));
    if (text) detail.textContent = `${JSON.stringify(item.input ?? {}, null, 2)}\n\n${text}`;
  };
  // A change waiting for the person's decision is not running.
  row.wait = () => {
    row.dataset.state = "waiting";
    statusIcon.replaceChildren(icon("shield-check", 14));
  };
  tools.set(item.id, row);
  return row;
}

function drawMessage(m) {
  if (m.role === "user") {
    for (const it of m.items.filter((x) => x.kind === "result")) tools.get(it.id)?.finish(it.error, it.text);
    const words = m.items.find((x) => x.kind === "text");
    return words ? el("div", { class: "fa-msg fa-user", "data-seq": m.seq }, el("div", { class: "fa-bubble", text: words.text })) : null;
  }
  const node = el("div", { class: "fa-msg fa-bot", "data-seq": m.seq });
  for (const it of m.items) {
    if (it.kind === "text" && it.text) node.append(rich(it.text));
    else if (it.kind === "thinking") node.append(el("details", { class: "fa-thinking" }, el("summary", { text: "Reasoning" }), rich(it.text)));
    else if (it.kind === "tool") node.append(toolRow(it));
  }
  return node.childElementCount ? node : null;
}

function note(text, tone = "info") {
  log.append(el("p", { class: `fa-note fa-${tone}`, role: tone === "error" ? "alert" : null }, tone === "error" ? icon("circle-alert", 14) : null, el("span", { text })));
  scroll();
}

function drawDraft() {
  const d = thread?.draft ?? {};
  if (!d.text && !d.thinking) {
    draftNode?.remove();
    draftNode = null;
    return;
  }
  if (!draftNode) draftNode = el("div", { class: "fa-msg fa-bot fa-draft" });
  draftNode.replaceChildren(
    ...(d.thinking ? [el("details", { class: "fa-thinking", open: !d.text }, el("summary", { text: "Reasoning…" }), rich(d.thinking))] : []),
    ...(d.text ? [rich(d.text)] : []),
  );
  log.append(draftNode);
  scroll();
}

function drawPending() {
  pendingNode?.remove();
  pendingNode = null;
  const calls = thread?.state === "waiting" ? (thread.pending?.calls ?? []) : [];
  if (!calls.length) return;
  for (const c of calls) tools.get(c.id)?.wait();
  const decide = (allow) => (e) => {
    for (const b of pendingNode.querySelectorAll("button")) b.disabled = true;
    e.currentTarget.textContent = allow ? "Allowing…" : "Refusing…";
    follow(`/api/assistant/threads/${thread.id}/decide`, { allow });
  };
  pendingNode = el(
    "section",
    { class: "fa-confirm", "aria-label": "Changes to approve" },
    el("p", { class: "fa-confirm-title" }, icon("shield-check", 16), el("span", { text: calls.length === 1 ? "The assistant wants to make this change:" : `The assistant wants to make ${calls.length} changes:` })),
    el(
      "ul",
      {},
      ...calls.map((c) =>
        el(
          "li",
          { class: c.destructive ? "fa-destructive" : null },
          el("strong", { text: `${c.title ?? c.name}${c.input?.entity ? ` · ${c.input.entity}` : ""}` }),
          el("pre", { text: JSON.stringify(c.input ?? {}, null, 2) }),
        ),
      ),
    ),
    el(
      "div",
      { class: "fa-confirm-actions" },
      el("button", { type: "button", class: "fa-btn", onclick: decide(false), text: "Don't allow" }),
      el("button", { type: "button", class: "fa-btn fa-primary", onclick: decide(true), text: calls.length === 1 ? "Allow" : "Allow all" }),
    ),
  );
  log.append(pendingNode);
  scroll();
  pendingNode.querySelector(".fa-primary")?.focus({ preventScroll: true });
}

const SUGGESTIONS = ["What needs my attention today?", "Which invoices are overdue?", "Summarize what changed this week", "Find a customer and their open orders"];

function drawThread() {
  tools.clear();
  draftNode = null;
  pendingNode = null;
  log.replaceChildren();
  titleEl.textContent = thread?.title || "Assistant";
  if (!thread?.messages?.length) {
    log.append(
      el(
        "div",
        { class: "fa-empty" },
        el("span", { class: "fa-mark" }, icon("sparkles", 22)),
        el("h2", { text: "How can I help?" }),
        el("p", { text: "Ask about your records, or ask me to do something. I work with your permissions, and I ask before changing anything." }),
        el(
          "div",
          { class: "fa-suggestions" },
          ...SUGGESTIONS.map((s) => el("button", { type: "button", class: "fa-chip", text: s, onclick: () => ((input.value = s), send()) })),
        ),
      ),
    );
  }
  for (const m of thread?.messages ?? []) {
    const n = drawMessage(m);
    if (n) log.append(n);
  }
  drawDraft();
  drawPending();
  setBusy();
  scroll(true);
}

function scroll(force = false) {
  const near = log.scrollHeight - log.scrollTop - log.clientHeight < 120;
  if (force || near) log.scrollTop = log.scrollHeight;
}

// ───────────────────────── events ─────────────────────────

function onEvent(ev) {
  if (!thread) return;
  switch (ev.type) {
    case "draft":
      thread.draft = ev.draft ?? {};
      drawDraft();
      break;
    case "delta": {
      thread.draft ??= {};
      thread.draft[ev.kind] = (thread.draft[ev.kind] ?? "") + ev.text;
      scheduleDraft();
      break;
    }
    case "restart":
      thread.draft = {};
      drawDraft();
      break;
    case "message": {
      thread.draft = {};
      drawDraft();
      log.querySelector(".fa-empty")?.remove();
      if (ev.message.role === "user") log.querySelector(".fa-sending")?.remove();
      if (!thread.messages.some((m) => m.seq === ev.message.seq)) {
        thread.messages.push(ev.message);
        const n = drawMessage(ev.message);
        if (n) log.append(n);
      }
      scroll();
      break;
    }
    case "tool":
      if (ev.status !== "running") tools.get(ev.id)?.finish(ev.status === "error", ev.text);
      break;
    case "confirm":
      thread.state = "waiting";
      thread.pending = ev.pending;
      drawPending();
      setBusy();
      break;
    case "note":
      note(ev.text);
      break;
    case "error":
      thread.draft = {};
      drawDraft();
      thread.state = "idle";
      note(ev.message ?? "The assistant could not answer.", "error");
      setBusy();
      refreshList();
      break;
    case "done":
      // What streamed without becoming a message (a stopped answer) was never kept.
      thread.draft = {};
      drawDraft();
      thread.state = ev.state ?? "idle";
      if (thread.state !== "waiting") {
        thread.pending = null;
        drawPending();
      }
      setBusy();
      refreshList();
      break;
    case "resync":
      load(thread.id);
      break;
  }
}

let draftFrame = 0;
function scheduleDraft() {
  if (draftFrame) return;
  draftFrame = requestAnimationFrame(() => {
    draftFrame = 0;
    drawDraft();
  });
}

/** Follows a turn's events; when the stream breaks while it still runs, asks now and then. */
async function follow(path, body) {
  following?.abort();
  const ctl = new AbortController();
  following = ctl;
  thread.state = "running";
  thread.pending = body?.allow === undefined ? thread.pending : null;
  if (body?.allow !== undefined) drawPending();
  setBusy();
  try {
    await events(path, body, ctl.signal);
  } catch (e) {
    if (ctl.signal.aborted) return;
    if (e.status === 409 || e.status === 403 || e.status === 422 || e.status === 429) {
      note(e.message, "error");
      return load(thread.id);
    }
  } finally {
    if (following === ctl) following = null;
  }
  // The stream ended without a word of the end (a proxy, a page in the background): ask.
  if (running()) watch();
}

function watch() {
  clearTimeout(poll);
  poll = setTimeout(async () => {
    if (!thread) return;
    const id = thread.id;
    try {
      const t = await api("GET", `/api/assistant/threads/${id}`);
      if (thread?.id !== id) return;
      const was = thread.messages.length;
      if (t.messages.length !== was || t.state !== thread.state) {
        thread = t;
        drawThread();
      }
      if (t.state === "running") watch();
    } catch {
      watch();
    }
  }, 2500);
}

// ───────────────────────── doing things ─────────────────────────

/** Where the person is, said to the model with their message. */
function context() {
  const lines = [];
  const now = new Date();
  let zone = "";
  try {
    zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  } catch {}
  lines.push(`Now: ${now.toLocaleString(undefined, { dateStyle: "full", timeStyle: "short" })}${zone ? ` (${zone})` : ""}`);
  try {
    if (inPanel) {
      const host = window.parent;
      lines.push(`Page: ${host.document.title} (${host.location.pathname})`);
    } else {
      lines.push("Page: the assistant's own page");
    }
  } catch {}
  // What the app that opened the panel pointed at (a record, a selection), from the runtime.
  const shown = shell()?.sidepanel?.context?.();
  if (shown) lines.push(`Shown with: ${JSON.stringify(shown).slice(0, 600)}`);
  return lines.join("\n");
}

async function send() {
  const text = input.value.trim();
  if (!text || running() || thread?.state === "waiting") return;
  try {
    if (!thread) {
      const made = await api("POST", "/api/assistant/threads");
      thread = { id: made.id, title: "", state: "idle", messages: [], pending: null, draft: {} };
      remember(made.id);
      drawThread();
    }
  } catch (e) {
    return note(e.message, "error");
  }
  input.value = "";
  resize();
  log.querySelector(".fa-empty")?.remove();
  log.append(el("div", { class: "fa-msg fa-user fa-sending" }, el("div", { class: "fa-bubble", text })));
  if (!thread.title) titleEl.textContent = text.slice(0, 80);
  scroll(true);
  follow(`/api/assistant/threads/${thread.id}/messages`, { text, context: context() });
}

async function stop() {
  if (!thread) return;
  try {
    await api("POST", `/api/assistant/threads/${thread.id}/stop`);
  } catch (e) {
    note(e.message, "error");
  }
}

async function load(id) {
  following?.abort();
  clearTimeout(poll);
  try {
    thread = await api("GET", `/api/assistant/threads/${id}`);
  } catch (e) {
    if (e.status === 404) {
      remember(null);
      thread = null;
      return drawThread();
    }
    return note(e.message, "error");
  }
  remember(id);
  drawThread();
  if (thread.state === "running") {
    if (thread.live) follow(`/api/assistant/threads/${id}/live`).catch(() => watch());
    else watch();
  }
}

function fresh() {
  following?.abort();
  clearTimeout(poll);
  thread = null;
  remember(null);
  closeMenu();
  drawThread();
  input.focus();
}

async function refreshList() {
  try {
    status = await api("GET", "/api/assistant");
    if (menuOpen) drawMenu();
  } catch {}
}

function drawMenu() {
  const list = status?.threads ?? [];
  const when = (at) => {
    try {
      return new Date(at).toLocaleDateString(undefined, { month: "short", day: "numeric" });
    } catch {
      return "";
    }
  };
  menu.replaceChildren(
    el("p", { class: "fa-menu-title", text: "Conversations" }),
    list.length
      ? el(
          "ul",
          {},
          ...list.map((t) =>
            el(
              "li",
              { "aria-current": t.id === thread?.id ? "true" : null },
              el(
                "button",
                { type: "button", class: "fa-thread", onclick: () => (closeMenu(), load(t.id)) },
                el("span", { class: "fa-thread-title", text: t.title || "New conversation" }),
                el("small", { text: t.state === "running" ? "Answering…" : t.state === "waiting" ? "Waiting for you" : when(t.updated_at) }),
              ),
              el(
                "button",
                {
                  type: "button",
                  class: "fa-icon-btn",
                  "aria-label": `Delete “${t.title || "New conversation"}”`,
                  title: "Delete",
                  onclick: async () => {
                    if (!confirm("Delete this conversation? It cannot be brought back.")) return;
                    try {
                      await api("DELETE", `/api/assistant/threads/${t.id}`);
                      if (thread?.id === t.id) fresh();
                      await refreshList();
                    } catch (e) {
                      note(e.message, "error");
                    }
                  },
                },
                icon("trash", 15),
              ),
            ),
          ),
        )
      : el("p", { class: "fa-muted", text: "No conversations yet." }),
    status?.admin ? el("button", { type: "button", class: "fa-link", text: "Turn the assistant off for this workspace", onclick: () => setWorkspace(false) }) : null,
  );
}

async function setWorkspace(on) {
  if (!on && !confirm("Turn the assistant off for everyone in this workspace? You can turn it on again here.")) return;
  try {
    await api("PUT", "/api/admin/settings", { assistant: on });
    closeMenu();
    // Every replica reads the setting again within a few seconds.
    setTimeout(start, 1500);
  } catch (e) {
    note(e.message, "error");
  }
}

function toggleMenu() {
  if (menuOpen) return closeMenu();
  menuOpen = true;
  drawMenu();
  menu.hidden = false;
  historyBtn.setAttribute("aria-expanded", "true");
  refreshList();
}

function closeMenu() {
  menuOpen = false;
  menu.hidden = true;
  historyBtn.setAttribute("aria-expanded", "false");
}

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && menuOpen) closeMenu();
});
document.addEventListener("pointerdown", (e) => {
  if (menuOpen && !menu.contains(e.target) && !historyBtn.contains(e.target)) closeMenu();
});

function resize() {
  input.style.height = "auto";
  input.style.height = `${Math.min(input.scrollHeight, 180)}px`;
}
input.addEventListener("input", resize);
input.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    send();
  }
});

// ───────────────────────── the page ─────────────────────────

const root = document.getElementById("main");

async function start() {
  try {
    status = await api("GET", "/api/assistant");
  } catch (e) {
    root.replaceChildren(el("div", { class: "fa-off" }, icon("circle-alert", 22), el("p", { text: e.message })));
    return shell()?.ready?.();
  }
  if (!status.available) {
    root.replaceChildren(
      el(
        "div",
        { class: "fa-off" },
        el("span", { class: "fa-mark" }, icon("sparkles", 22)),
        el("h2", { text: "The assistant is not available" }),
        el("p", { text: status.reason ?? "" }),
        status.admin && /turned the assistant off/.test(status.reason ?? "") ? el("button", { type: "button", class: "fa-btn fa-primary", text: "Turn it on", onclick: () => setWorkspace(true) }) : null,
      ),
    );
    return shell()?.ready?.();
  }
  root.replaceChildren(
    el("header", { class: "fa-head" }, el("span", { class: "fa-mark fa-small" }, icon("sparkles", 15)), titleEl, historyBtn, newBtn, menu),
    log,
    form,
  );
  if (!inPanel) shell()?.title?.("Assistant");
  const last = remembered() ?? status.threads?.find((t) => t.state !== "idle")?.id;
  if (last) await load(last);
  else drawThread();
  shell()?.ready?.();
  if (!inPanel || !thread) input.focus({ preventScroll: true });
}

start();
