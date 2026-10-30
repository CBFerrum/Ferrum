// The shell runtime, v1 (docs/plan/apps-ui.md §10.3, §12.2). The server puts it on every page of
// every app, before the app's own scripts: it draws <ferrum-bar> (launcher, title, account) in a
// shadow root, and installs globalThis.ferrum.shell, the one API an app talks to whatever hosts it.
// No framework; nothing here may assume which app runs below it.

const VERSION = "1.0.0";
const START_TIMEOUT = 10000;

function readBootstrap() {
  try {
    return JSON.parse(document.getElementById("ferrum-bootstrap")?.textContent ?? "null");
  } catch {
    return null;
  }
}

const boot = readBootstrap();
const params = new URLSearchParams(location.search);
const inFrame = window.top !== window;
const intentId = params.get("_intent");
const companionKind = window.name.startsWith("ferrum-companion-") ? window.name.slice("ferrum-companion-".length) : null;
const hostKind = inFrame ? "frame" : companionKind ? "companion" : (boot?.host?.kind ?? "page");
const appTitle = boot?.app?.title ?? "Ferrum";
const events = new EventTarget();

const memory = {
  get(store, k) {
    try {
      return JSON.parse(store.getItem(k) ?? "null");
    } catch {
      return null;
    }
  },
  set(store, k, v) {
    try {
      store.setItem(k, JSON.stringify(v));
    } catch {}
  },
};

// ───────────────────────── sessions ─────────────────────────

const sessionChannel = "BroadcastChannel" in window ? new BroadcastChannel("ferrum-session") : null;

function toLogin() {
  location.assign(`/login?next=${encodeURIComponent(location.pathname + location.search)}`);
}

sessionChannel?.addEventListener("message", (e) => {
  if (e.data?.type === "signed-out") {
    events.dispatchEvent(new CustomEvent("signed-out"));
    toLogin();
  }
});

async function signOut() {
  await fetch("/api/auth/logout", { method: "POST", credentials: "same-origin" }).catch(() => {});
  try {
    sessionStorage.removeItem("ferrum.session");
    sessionStorage.removeItem("ferrum.shell");
  } catch {}
  sessionChannel?.postMessage({ type: "signed-out" });
  location.assign("/login");
}

// ───────────────────────── the launcher's data ─────────────────────────

let shellData = memory.get(sessionStorage, "ferrum.shell")?.data ?? null;
let shellLoading = null;

function loadShell() {
  shellLoading ??= (async () => {
    const cached = memory.get(sessionStorage, "ferrum.shell");
    const r = await fetch("/api/shell", { credentials: "same-origin", headers: cached?.etag ? { "if-none-match": cached.etag } : {} }).catch(() => null);
    if (r?.status === 304 && cached) shellData = cached.data;
    else if (r?.ok) {
      shellData = await r.json();
      memory.set(sessionStorage, "ferrum.shell", { etag: r.headers.get("etag"), data: shellData });
    } else if (r?.status === 401) {
      toLogin();
    }
    shellLoading = null;
    events.dispatchEvent(new CustomEvent("shell"));
    return shellData;
  })();
  return shellLoading;
}

// ───────────────────────── the bar ─────────────────────────

const CSS = `
:host { all: initial; display: block; position: sticky; top: 0; z-index: 2147483000; height: var(--ferrum-bar-height, 48px);
  font: 14px/1.4 var(--ferrum-font, system-ui, sans-serif); color: var(--ferrum-text, #17191d); }
:host([hidden]), [hidden] { display: none !important; }
* { box-sizing: border-box; }
.bar { display: flex; align-items: center; gap: 6px; height: 100%; padding: 0 8px; background: var(--ferrum-surface, #fff); border-bottom: 1px solid var(--ferrum-border, #d9dee5); }
button, a.btn { display: inline-grid; place-items: center; min-width: 32px; height: 32px; padding: 0 8px; border: 1px solid transparent; border-radius: 6px;
  background: transparent; color: inherit; font: inherit; text-decoration: none; cursor: pointer; }
button:hover, a.btn:hover { background: var(--ferrum-surface-2, #eef1f5); }
button:focus-visible, a:focus-visible, input:focus-visible { outline: none; box-shadow: var(--ferrum-focus-ring, 0 0 0 3px rgb(47 91 234 / .35)); }
.home { font-weight: 650; color: inherit; text-decoration: none; padding: 0 6px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 16em; }
.sep { width: 1px; height: 20px; background: var(--ferrum-border, #d9dee5); }
.title { font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; min-width: 0; }
.crumbs { color: var(--ferrum-muted, #5b6472); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; min-width: 0; }
.spacer { flex: 1; }
.me { width: 32px; border-radius: 50%; background: var(--ferrum-primary-soft, #e7edfd); color: var(--ferrum-primary, #2f5bea); font-weight: 650; font-size: 12px; }
.indicator { display: inline-flex; align-items: center; gap: 6px; padding: 2px 8px; border-radius: 999px; background: var(--ferrum-warning-soft, #fff3d6); font-size: 12px; }
.panel { position: absolute; top: calc(var(--ferrum-bar-height, 48px) - 4px); background: var(--ferrum-surface, #fff); border: 1px solid var(--ferrum-border, #d9dee5);
  border-radius: 8px; box-shadow: var(--ferrum-shadow, 0 8px 28px rgb(16 24 40 / .14)); padding: 10px; max-height: calc(100vh - 64px); overflow: auto; }
.launcher { left: 8px; width: min(420px, calc(100vw - 16px)); }
.menu { right: 8px; min-width: 220px; }
.launcher > .empty a { color: var(--ferrum-primary, #2f5bea); }
.launcher input { width: 100%; height: 34px; padding: 0 10px; margin-bottom: 10px; border: 1px solid var(--ferrum-border, #d9dee5); border-radius: 6px;
  background: var(--ferrum-surface, #fff); color: inherit; font: inherit; }
.apps { display: grid; grid-template-columns: repeat(auto-fill, minmax(96px, 1fr)); gap: 4px; }
.app { display: grid; justify-items: center; gap: 6px; padding: 10px 4px; border-radius: 8px; color: inherit; text-decoration: none; text-align: center; font-size: 13px; }
.app:hover, .app:focus-visible { background: var(--ferrum-surface-2, #eef1f5); }
.app[aria-current="page"] { background: var(--ferrum-primary-soft, #e7edfd); }
.tile { display: grid; place-items: center; width: 40px; height: 40px; border-radius: 10px; color: #fff; font-weight: 700; font-size: 17px; }
.empty { color: var(--ferrum-muted, #5b6472); padding: 8px; }
.menu .who { padding: 6px 8px 10px; border-bottom: 1px solid var(--ferrum-border, #d9dee5); margin-bottom: 6px; }
.menu .who small { display: block; color: var(--ferrum-muted, #5b6472); }
.menu a, .menu button { display: flex; width: 100%; justify-content: flex-start; color: inherit; text-decoration: none; }
.notice { position: absolute; top: calc(var(--ferrum-bar-height, 48px) + 8px); left: 50%; transform: translateX(-50%); display: flex; gap: 10px; align-items: center;
  max-width: calc(100vw - 16px); padding: 8px 12px; border-radius: 8px; background: var(--ferrum-text, #17191d); color: var(--ferrum-surface, #fff); box-shadow: var(--ferrum-shadow); }
.notice button { color: inherit; border-color: currentColor; height: 28px; }
.skip { position: absolute; left: -9999px; }
.skip:focus { left: 8px; top: 8px; z-index: 1; padding: 6px 10px; background: var(--ferrum-surface, #fff); border-radius: 6px; }
.banner { position: absolute; top: var(--ferrum-bar-height, 48px); left: 0; right: 0; padding: 4px 12px; text-align: center; font-size: 12px;
  background: var(--ferrum-warning-soft, #fff3d6); }
@media (max-width: 640px) { .home, .sep, .crumbs { display: none; } .launcher { left: 4px; right: 4px; width: auto; } }
`;

const GRID = `<svg width="18" height="18" viewBox="0 0 18 18" aria-hidden="true" fill="currentColor"><circle cx="3" cy="3" r="2"/><circle cx="9" cy="3" r="2"/><circle cx="15" cy="3" r="2"/><circle cx="3" cy="9" r="2"/><circle cx="9" cy="9" r="2"/><circle cx="15" cy="9" r="2"/><circle cx="3" cy="15" r="2"/><circle cx="9" cy="15" r="2"/><circle cx="15" cy="15" r="2"/></svg>`;

function initials(name) {
  return (
    (name ?? "")
      .split(/\s+/)
      .filter(Boolean)
      .slice(0, 2)
      .map((w) => w[0].toUpperCase())
      .join("") || "?"
  );
}

/** A launcher tile's colour: the app's own, else one derived from its name. */
function colourOf(entry) {
  if (/^#[0-9a-f]{6}$/i.test(entry.color ?? "")) return entry.color;
  let h = 0;
  for (const c of entry.title ?? entry.app ?? "") h = (h * 31 + c.charCodeAt(0)) % 360;
  return `hsl(${h} 55% 42%)`;
}

const el = (tag, attrs = {}, ...children) => {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "text") e.textContent = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v === true ? "" : v);
  }
  e.append(...children.filter(Boolean));
  return e;
};

class FerrumBar extends HTMLElement {
  connectedCallback() {
    if (this.shadowRoot) return;
    if (hostKind === "frame") {
      this.hidden = true;
      document.documentElement.style.setProperty("--ferrum-bar-height", "0px");
      return;
    }
    if (boot?.app?.compact || hostKind === "companion") this.setAttribute("compact", "");
    const root = this.attachShadow({ mode: "open" });
    const style = document.createElement("style");
    style.textContent = CSS;
    const person = boot?.person ?? {};
    this.titleEl = el("span", { class: "title", "aria-live": "polite", text: boot?.app ? appTitle : "" });
    this.crumbsEl = el("span", { class: "crumbs" });
    this.indicators = el("span", { class: "indicators" });
    this.launchBtn = el("button", { class: "launch", "aria-label": "Apps", "aria-haspopup": "dialog", "aria-expanded": "false", onclick: () => this.toggleLauncher() });
    this.launchBtn.innerHTML = GRID;
    this.meBtn = el("button", {
      class: "me",
      "aria-label": `Account: ${person.name ?? ""}`,
      "aria-haspopup": "menu",
      "aria-expanded": "false",
      text: initials(person.name),
      onclick: () => this.toggleMenu(),
    });
    const bar = el(
      "div",
      { class: "bar", role: "banner" },
      el("a", { class: "skip", href: "#main", text: "Skip to content", onclick: (e) => this.skip(e) }),
      this.launchBtn,
      el("a", { class: "home", href: "/", text: boot?.workspace?.name ?? "Home" }),
      el("span", { class: "sep", "aria-hidden": "true" }),
      this.titleEl,
      this.crumbsEl,
      el("span", { class: "spacer" }),
      this.indicators,
      this.meBtn,
    );
    this.launcher = el("div", { class: "panel launcher", role: "dialog", "aria-label": "Apps", hidden: true });
    this.menu = el("div", { class: "panel menu", role: "menu", "aria-label": "Account", hidden: true });
    this.noticeEl = el("div", { class: "notice", role: "status", hidden: true });
    root.append(style, bar, this.launcher, this.menu, this.noticeEl);
    if (person.actor) root.append(el("div", { class: "banner", role: "note", text: `You are acting as ${person.name}. Everything you do is recorded with both names.` }));
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.closePanels(true);
    });
    document.addEventListener("pointerdown", (e) => {
      if (!e.composedPath().includes(this)) this.closePanels(false);
    });
    events.addEventListener("shell", () => {
      if (!this.launcher.hidden) this.drawLauncher();
    });
    // The launcher's data, early but not in the way.
    (window.requestIdleCallback ?? ((f) => setTimeout(f, 1500)))(() => loadShell());
  }

  skip(e) {
    e.preventDefault();
    const main = document.getElementById("main") ?? document.querySelector("main, [role=main]");
    if (main) {
      if (!main.hasAttribute("tabindex")) main.setAttribute("tabindex", "-1");
      main.focus();
    }
  }

  closePanels(restoreFocus) {
    const wasOpen = !this.launcher.hidden ? this.launchBtn : !this.menu.hidden ? this.meBtn : null;
    this.launcher.hidden = true;
    this.menu.hidden = true;
    this.launchBtn.setAttribute("aria-expanded", "false");
    this.meBtn.setAttribute("aria-expanded", "false");
    if (restoreFocus && wasOpen) wasOpen.focus();
  }

  async toggleLauncher() {
    const open = this.launcher.hidden;
    this.closePanels(false);
    if (!open) return;
    this.launcher.hidden = false;
    this.launchBtn.setAttribute("aria-expanded", "true");
    this.drawLauncher();
    if (!shellData) await loadShell();
    this.launcher.querySelector("input")?.focus();
  }

  drawLauncher() {
    const search = el("input", { type: "search", placeholder: "Find an app", "aria-label": "Find an app" });
    const grid = el("div", { class: "apps", role: "list" });
    const draw = () => {
      const q = search.value.trim().toLowerCase();
      const apps = (shellData?.apps ?? []).filter((a) => !q || `${a.title} ${a.description ?? ""}`.toLowerCase().includes(q));
      grid.replaceChildren(
        ...apps.map((a) => {
          const here = a.url !== "/" && location.pathname.startsWith(a.url);
          const link = el(
            "a",
            { class: "app", href: a.url, role: "listitem", "aria-current": here ? "page" : null, title: a.description ?? null },
            el("span", { class: "tile", "aria-hidden": "true", style: `background:${colourOf(a)}`, text: (a.title ?? "?").slice(0, 1).toUpperCase() }),
            el("span", { text: a.title }),
          );
          link.addEventListener("pointerenter", () => speculate(a.url), { once: true });
          link.addEventListener("focus", () => speculate(a.url), { once: true });
          return link;
        }),
      );
      if (!apps.length) grid.append(el("p", { class: "empty", text: shellData ? (q ? "No app matches." : "No apps yet.") : "Loading…" }));
    };
    search.addEventListener("input", draw);
    search.addEventListener("keydown", (e) => {
      if (e.key === "Enter") grid.querySelector("a")?.click();
    });
    draw();
    const footer = el("p", { class: "empty" }, el("a", { href: shellData?.legacy ?? "/app/", text: "Classic interface" }));
    this.launcher.replaceChildren(search, grid, footer);
  }

  toggleMenu() {
    const open = this.menu.hidden;
    this.closePanels(false);
    if (!open) return;
    const person = boot?.person ?? {};
    this.menu.replaceChildren(
      el("div", { class: "who" }, el("strong", { text: person.name ?? "" }), el("small", { text: person.email ?? "" }), el("small", { text: boot?.workspace?.name ?? "" })),
      el("a", { role: "menuitem", href: "/app/settings", text: "Account and devices" }),
      el("a", { role: "menuitem", href: shellData?.legacy ?? "/app/", text: "Classic interface" }),
      el("button", { role: "menuitem", type: "button", text: "Sign out", onclick: () => guardedLeave(signOut) }),
    );
    this.menu.hidden = false;
    this.meBtn.setAttribute("aria-expanded", "true");
    this.menu.querySelector("a, button")?.focus();
  }

  notice(text, actions = []) {
    if (!this.noticeEl) return;
    this.noticeEl.replaceChildren(el("span", { text }), ...actions.map(([label, run]) => el("button", { type: "button", text: label, onclick: run })));
    this.noticeEl.hidden = false;
  }

  hideNotice() {
    if (this.noticeEl) this.noticeEl.hidden = true;
  }

  setIndicator(key, text, run) {
    if (!this.indicators) return;
    this.indicators.querySelector(`[data-key="${key}"]`)?.remove();
    if (text) this.indicators.append(el("button", { class: "indicator", "data-key": key, type: "button", text, onclick: run }));
  }
}

customElements.define("ferrum-bar", FerrumBar);

const bar = () => document.querySelector("ferrum-bar");

// ───────────────────────── moving between apps ─────────────────────────

/** Prerender (Chromium) or prefetch an app's page the person is about to open. */
function speculate(url) {
  if (!url || url.startsWith(location.pathname)) return;
  if (HTMLScriptElement.supports?.("speculationrules")) {
    const s = document.createElement("script");
    s.type = "speculationrules";
    s.textContent = JSON.stringify({ prerender: [{ source: "list", urls: [url] }] });
    document.head.append(s);
  } else {
    document.head.append(el("link", { rel: "prefetch", href: url }));
  }
}

const guards = new Set();
const dirty = () => [...guards].some((g) => {
  try {
    return g();
  } catch {
    return false;
  }
});

// Listening for beforeunload keeps a page out of Firefox's back/forward cache: only while an app
// has a leave guard registered.
const onBeforeUnload = (e) => {
  if (dirty()) e.preventDefault();
};
function watchLeaving() {
  if (guards.size) window.addEventListener("beforeunload", onBeforeUnload);
  else window.removeEventListener("beforeunload", onBeforeUnload);
}

function guardedLeave(go) {
  if (dirty() && !confirm("Leave this page? Changes you made may not be saved.")) return false;
  guards.clear();
  watchLeaving();
  go();
  return true;
}

function navigate(target) {
  let url;
  if (typeof target === "string") url = target;
  else if (target?.entity && target?.id) url = `/open/${encodeURIComponent(target.entity)}/${encodeURIComponent(target.id)}${boot?.app?.id ? `?via=${encodeURIComponent(boot.app.id)}` : ""}`;
  else if (target?.path) url = target.path;
  if (!url) throw new TypeError("navigate: a path, a URL or {entity, id}");
  if (inFrame) {
    // A frame asks its host to go there.
    window.parent.postMessage({ type: "ferrum:navigate", url }, location.origin);
    return true;
  }
  return guardedLeave(() => location.assign(url));
}

// ───────────────────────── intents (§11.3) ─────────────────────────

async function handlersFor(action, type) {
  const data = shellData ?? (await loadShell());
  return (data?.intents ?? []).filter((i) => i.action === action && i.type === type);
}

function intent({ action, type, data } = {}) {
  return new Promise((resolve, reject) => {
    handlersFor(action, type).then((handlers) => {
      if (!handlers.length) {
        const err = new Error(`no app handles ${action} ${type}`);
        err.code = "no_handler";
        reject(err);
        return;
      }
      // ponytail: the first handler; a chooser with "always use" when several apps offer one.
      const h = handlers[0];
      const id = crypto.randomUUID();
      const dialog = document.createElement("dialog");
      Object.assign(dialog.style, {
        width: "min(880px, calc(100vw - 32px))",
        height: "min(640px, calc(100vh - 64px))",
        padding: "0",
        border: "1px solid var(--ferrum-border, #d9dee5)",
        borderRadius: "10px",
        background: "var(--ferrum-surface, #fff)",
        boxShadow: "var(--ferrum-shadow)",
      });
      dialog.setAttribute("aria-label", h.title ?? `${action} ${type}`);
      const frame = document.createElement("iframe");
      Object.assign(frame.style, { width: "100%", height: "100%", border: "0", display: "block" });
      frame.title = h.title ?? `${action} ${type}`;
      const sep = h.url.includes("?") ? "&" : "?";
      frame.src = `${h.url}${sep}_intent=${encodeURIComponent(id)}`;
      const channel = new MessageChannel();
      let settled = false;
      const finish = (outcome) => {
        if (settled) return;
        settled = true;
        window.removeEventListener("message", onMessage);
        channel.port1.close();
        if (dialog.open) dialog.close();
        dialog.remove();
        resolve(outcome);
      };
      const onMessage = (e) => {
        if (e.origin !== location.origin || e.source !== frame.contentWindow) return;
        if (e.data?.type === "ferrum:intent-ready" && e.data.id === id) {
          frame.contentWindow.postMessage({ type: "ferrum:intent", id, action, intentType: type, data: data ?? null, from: boot?.app?.id ?? null }, location.origin, [channel.port2]);
        } else if (e.data?.type === "ferrum:navigate" && typeof e.data.url === "string" && e.data.url.startsWith("/")) {
          finish({ status: "cancelled" });
          navigate(e.data.url);
        }
      };
      channel.port1.onmessage = (e) => {
        if (e.data?.done) finish({ status: "done", result: e.data.result ?? null, app: h.app });
        else if (e.data?.cancel) finish({ status: "cancelled" });
      };
      window.addEventListener("message", onMessage);
      dialog.addEventListener("close", () => finish({ status: "cancelled" }));
      dialog.append(frame);
      document.body.append(dialog);
      dialog.showModal();
    }, reject);
  });
}

// The handler's side: a page opened by an intent learns what it was asked and answers once.
let intentPort = null;
const intentRequest =
  inFrame && intentId
    ? new Promise((resolve) => {
        const onMessage = (e) => {
          if (e.origin !== location.origin || e.source !== window.parent || e.data?.type !== "ferrum:intent" || e.data.id !== intentId) return;
          window.removeEventListener("message", onMessage);
          intentPort = e.ports[0] ?? null;
          resolve({ action: e.data.action, type: e.data.intentType, data: e.data.data, from: e.data.from });
        };
        window.addEventListener("message", onMessage);
        window.parent.postMessage({ type: "ferrum:intent-ready", id: intentId }, location.origin);
      })
    : null;

intent.request = () => intentRequest ?? Promise.reject(new Error("this page was not opened for an intent"));
intent.done = (result) => intentPort?.postMessage({ done: true, result: result ?? null });
intent.cancel = () => intentPort?.postMessage({ cancel: true });

async function canIntent(action, type) {
  return (await handlersFor(action, type)).length > 0;
}

// ───────────────────────── companion windows (§12.6) ─────────────────────────

const companions = "BroadcastChannel" in window ? new BroadcastChannel("ferrum-companion") : null;
const companionsSeen = new Map();

function companion({ kind, url, width = 960, height = 640 } = {}) {
  if (!/^[a-z][a-z0-9-]{0,30}$/.test(kind ?? "") || typeof url !== "string" || !url.startsWith("/")) throw new TypeError("companion: {kind, url}");
  const w = window.open(url, `ferrum-companion-${kind}`, `popup,width=${width},height=${height}`);
  if (!w) {
    // Blocked: here instead, with a warning that leaving ends it.
    bar()?.notice("The window was blocked; it opens here instead. Leaving this page will end it.");
    location.assign(url);
    return null;
  }
  w.focus();
  return w;
}

if (companionKind) {
  // This window is a companion: tell every other page it is here, and come forward when asked.
  setInterval(() => companions?.postMessage({ type: "alive", kind: companionKind, title: document.title }), 2000);
  companions?.addEventListener("message", (e) => {
    if (e.data?.type === "focus" && e.data.kind === companionKind) window.focus();
  });
  window.addEventListener("pagehide", () => companions?.postMessage({ type: "gone", kind: companionKind }));
} else {
  companions?.addEventListener("message", (e) => {
    const { type, kind, title } = e.data ?? {};
    if (!kind) return;
    if (type === "alive") companionsSeen.set(kind, Date.now());
    if (type === "gone") companionsSeen.delete(kind);
    bar()?.setIndicator(`companion-${kind}`, companionsSeen.has(kind) ? `${title || kind} · Return` : "", () => companions?.postMessage({ type: "focus", kind }));
  });
  setInterval(() => {
    for (const [kind, at] of companionsSeen) {
      if (Date.now() - at > 6000) {
        companionsSeen.delete(kind);
        bar()?.setIndicator(`companion-${kind}`, "");
      }
    }
  }, 3000);
}

// ───────────────────────── the live channel (§12.4) ─────────────────────────

let livePort = null;
const liveHandlers = new Set();

function livePortOf() {
  if (livePort) return livePort;
  const url = "/_shell/v1/live-worker.js";
  if ("SharedWorker" in window) {
    const w = new SharedWorker(url, { name: "ferrum-live" });
    livePort = w.port;
    livePort.start();
  } else {
    livePort = new Worker(url, { name: "ferrum-live" });
  }
  livePort.onmessage = (e) => {
    if (e.data?.type === "state" && e.data.state === "signed-out") toLogin();
    for (const h of liveHandlers) h(e.data);
  };
  // A page leaving for good says so; a page put in the back/forward cache comes back with pageshow.
  window.addEventListener("pagehide", (e) => {
    if (!e.persisted) livePort?.postMessage({ type: "bye" });
  });
  return livePort;
}

function subscribe(topics, onChange) {
  const list = (Array.isArray(topics) ? topics : [topics]).filter((t) => typeof t === "string");
  const port = livePortOf();
  const handler = (msg) => {
    if (msg?.type !== "message") return;
    const d = msg.data;
    const topics = d.entity ? [d.entity] : d.notification ? ["notification"] : d.push ? ["push", `push:${d.push.kind ?? ""}`] : [];
    if (topics.some((x) => list.includes(x))) onChange(d);
  };
  liveHandlers.add(handler);
  port.postMessage({ type: "subscribe", topics: list });
  return () => {
    liveHandlers.delete(handler);
    port.postMessage({ type: "unsubscribe", topics: list });
  };
}

window.addEventListener("pageshow", (e) => {
  // Back from the back/forward cache: the worker may have forgotten this page.
  if (e.persisted && livePort) livePort.postMessage({ type: "subscribe", topics: [] });
});

// ───────────────────────── start-up and versions (§12.12) ─────────────────────────

let started = false;
const startTimer =
  hostKind === "page" && boot?.app
    ? setTimeout(() => {
        if (started) return;
        const key = `ferrum.reloaded.${boot.app.release}`;
        if (!sessionStorage.getItem(key)) {
          sessionStorage.setItem(key, "1");
          location.reload();
          return;
        }
        bar()?.notice(`${appTitle} did not start.`, [["Reload", () => location.reload()]]);
      }, START_TIMEOUT)
    : null;

function ready() {
  if (started) return;
  started = true;
  clearTimeout(startTimer);
  // A standard mark: tests and real-user timing read it whenever they look.
  performance.mark?.("ferrum:ready");
  if (boot?.app) sessionStorage.removeItem(`ferrum.reloaded.${boot.app.release}`);
  events.dispatchEvent(new CustomEvent("ready", { detail: { ms: Math.round(performance.now()) } }));
}

let outdated = false;
async function checkVersion() {
  if (!boot?.app?.id || outdated) return;
  const r = await fetch(`/api/shell/version?app=${encodeURIComponent(boot.app.id)}`, { credentials: "same-origin" }).catch(() => null);
  const v = await r?.json().catch(() => null);
  if (v?.release && v.release !== boot.app.release) {
    outdated = true;
    events.dispatchEvent(new CustomEvent("outdated", { detail: v }));
    bar()?.notice(`A new version of ${appTitle} is available.`, [["Reload", () => guardedLeave(() => location.reload())]]);
  }
}

// A file of this release that will not load (removed after an upgrade) means a newer one runs.
window.addEventListener(
  "error",
  (e) => {
    const src = e.target?.src ?? e.target?.href;
    if (typeof src === "string" && src.includes("/_ui/")) checkVersion();
  },
  true,
);
window.addEventListener("unhandledrejection", (e) => {
  if (/dynamically imported module|Importing a module script failed|error loading dynamically imported/i.test(String(e.reason?.message ?? e.reason))) checkVersion();
});
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") checkVersion();
});

window.addEventListener("offline", () => bar()?.setIndicator("offline", "Offline", () => {}));
window.addEventListener("online", () => bar()?.setIndicator("offline", ""));

// ───────────────────────── the API ─────────────────────────

const CAPABILITIES = new Set(["navigate", "title", "breadcrumbs", "badge", "intent", "canIntent", "companion", "toast", "leaveGuard", "session", "theme", "locale", "live", "ready"]);

function toast(message, { tone = "info" } = {}) {
  const b = bar();
  if (!b?.notice) return;
  b.notice(String(message));
  setTimeout(() => b.hideNotice(), tone === "error" ? 8000 : 4000);
}

const shell = Object.freeze({
  version: VERSION,
  host: hostKind,
  bootstrap: boot,
  supports: (capability) => CAPABILITIES.has(capability),
  ready,
  navigate,
  title(text) {
    const t = text ? String(text) : "";
    const b = bar();
    if (b?.titleEl) b.titleEl.textContent = t || appTitle;
    document.title = t && t !== appTitle ? `${t} · ${appTitle}` : appTitle;
  },
  breadcrumbs(list) {
    const b = bar();
    if (!b?.crumbsEl) return;
    // The trail replaces the title: it ends with where the person is.
    const trail = (Array.isArray(list) ? list : []).map((c) => (typeof c === "string" ? c : c?.label ?? "")).filter(Boolean);
    b.crumbsEl.textContent = trail.join(" › ");
    b.titleEl.hidden = trail.length > 0;
  },
  badge(count) {
    const n = Number(count) || 0;
    if (n > 0) navigator.setAppBadge?.(n).catch(() => {});
    else navigator.clearAppBadge?.().catch(() => {});
  },
  intent,
  canIntent,
  companion,
  toast,
  leaveGuard(isDirty) {
    guards.add(isDirty);
    watchLeaving();
    return () => {
      guards.delete(isDirty);
      watchLeaving();
    };
  },
  session: Object.freeze({
    signOut: () => guardedLeave(signOut),
    reauthenticate: () => guardedLeave(toLogin),
    on: (event, cb) => events.addEventListener(event, (e) => cb(e.detail)),
  }),
  theme: Object.freeze({ ...(boot?.theme ?? {}) }),
  locale: Object.freeze({ ...(boot?.locale ?? {}) }),
  live: Object.freeze({ subscribe }),
  apps: () => loadShell().then((d) => d?.apps ?? []),
  on: (event, cb) => events.addEventListener(event, (e) => cb(e.detail)),
});

const ns = globalThis.ferrum && typeof globalThis.ferrum === "object" ? globalThis.ferrum : {};
Object.defineProperty(globalThis, "ferrum", { value: Object.freeze({ ...ns, shell }), configurable: false, writable: false });
document.documentElement.dataset.ferrumHost = hostKind;
