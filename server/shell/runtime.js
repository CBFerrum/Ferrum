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
// A side panel's page (§11.4): its id, and the context it was opened with.
const panelId = inFrame ? params.get("_panel") : null;
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
  // A public page has nobody to sign out.
  if (e.data?.type === "signed-out" && hostKind !== "public") {
    events.dispatchEvent(new CustomEvent("signed-out"));
    toLogin();
  }
});

async function signOut() {
  const pending = [...uploads.values()].filter((u) => mine(u) && !FINISHED.has(u.state));
  if (pending.length && !confirm(`${pending.length === 1 ? "An upload has" : `${pending.length} uploads have`} not finished. Sign out and stop ${pending.length === 1 ? "it" : "them"}?`)) return;
  await forgetUploads();
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
.group { grid-column: 1 / -1; margin: 8px 4px 0; font-size: 12px; font-weight: 650; color: var(--ferrum-muted, #5b6472); }
.tools { display: inline-flex; align-items: center; gap: 2px; }
.tools button[aria-pressed="true"] { background: var(--ferrum-primary-soft, #e7edfd); color: var(--ferrum-primary, #2f5bea); }
.create { right: 8px; min-width: 200px; }
.tray { right: 8px; width: min(380px, calc(100vw - 16px)); }
.tray ul { list-style: none; margin: 0; padding: 0; display: grid; gap: 8px; }
.tray li { display: grid; gap: 4px; }
.tray .row { display: flex; gap: 6px; align-items: center; }
.tray .name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tray small { color: var(--ferrum-muted, #5b6472); }
.tray .err { color: var(--ferrum-danger, #c62828); }
.tray progress { width: 100%; height: 6px; }
.tray .actions { display: flex; justify-content: flex-end; margin-top: 8px; }
.side { position: fixed; top: var(--ferrum-bar-height, 48px); right: 0; bottom: 0; width: var(--ferrum-panel-width, 360px); max-width: 100vw;
  display: flex; flex-direction: column; background: var(--ferrum-surface, #fff); border-left: 1px solid var(--ferrum-border, #d9dee5); box-shadow: var(--ferrum-shadow); }
.side header { display: flex; align-items: center; gap: 4px; height: 40px; padding: 0 4px 0 12px; border-bottom: 1px solid var(--ferrum-border, #d9dee5); }
.side header strong { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.side iframe { flex: 1; width: 100%; border: 0; background: var(--ferrum-surface, #fff); }
.grip { position: absolute; left: -4px; top: 0; bottom: 0; width: 8px; cursor: ew-resize; touch-action: none; }
@media (max-width: 640px) { .home, .sep, .crumbs { display: none; } .launcher { left: 4px; right: 4px; width: auto; } }
`;

const GRID = `<svg width="18" height="18" viewBox="0 0 18 18" aria-hidden="true" fill="currentColor"><circle cx="3" cy="3" r="2"/><circle cx="9" cy="3" r="2"/><circle cx="15" cy="3" r="2"/><circle cx="3" cy="9" r="2"/><circle cx="9" cy="9" r="2"/><circle cx="15" cy="9" r="2"/><circle cx="3" cy="15" r="2"/><circle cx="9" cy="15" r="2"/><circle cx="15" cy="15" r="2"/></svg>`;

const PLUS = `<svg width="18" height="18" viewBox="0 0 18 18" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M9 3v12M3 9h12"/></svg>`;
const CLOSE = `<svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M4 4l8 8M12 4l-8 8"/></svg>`;
const OPEN = `<svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M9 3h4v4M13 3L7 9M12 9.5V13H3V4h3.5"/></svg>`;

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
      (this.tools = el("span", { class: "tools" })),
      this.meBtn,
    );
    this.launcher = el("div", { class: "panel launcher", role: "dialog", "aria-label": "Apps", hidden: true });
    this.menu = el("div", { class: "panel menu", role: "menu", "aria-label": "Account", hidden: true });
    this.createMenu = el("div", { class: "panel menu create", role: "menu", "aria-label": "Create", hidden: true });
    this.tray = el("div", { class: "panel tray", role: "dialog", "aria-label": "Uploads", hidden: true });
    this.side = el("aside", { class: "side", "aria-label": "Side panel", hidden: true });
    this.noticeEl = el("div", { class: "notice", role: "status", hidden: true });
    root.append(style, bar, this.launcher, this.menu, this.createMenu, this.tray, this.side, this.noticeEl);
    if (person.actor) root.append(el("div", { class: "banner", role: "note", text: `You are acting as ${person.name}. Everything you do is recorded with both names.` }));
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.closePanels(true);
    });
    document.addEventListener("pointerdown", (e) => {
      if (!e.composedPath().includes(this)) this.closePanels(false);
    });
    events.addEventListener("shell", () => {
      if (!this.launcher.hidden) this.drawLauncher();
      this.drawTools();
    });
    this.drawTools();
    // The launcher's data, early but not in the way; at once when a side panel was open in this tab.
    if (rememberedPanel()) loadShell().then(() => (rememberedPanel() && !openPanel ? openSidepanel(rememberedPanel()) : null));
    else (window.requestIdleCallback ?? ((f) => setTimeout(f, 1500)))(() => loadShell());
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
    const wasOpen = !this.launcher.hidden ? this.launchBtn : !this.menu.hidden ? this.meBtn : !this.createMenu.hidden ? this.createBtn : null;
    this.launcher.hidden = true;
    this.menu.hidden = true;
    this.createMenu.hidden = true;
    this.tray.hidden = true;
    this.launchBtn.setAttribute("aria-expanded", "false");
    this.meBtn.setAttribute("aria-expanded", "false");
    this.createBtn?.setAttribute("aria-expanded", "false");
    if (restoreFocus && wasOpen) wasOpen.focus();
  }

  /** The Create menu and the side panels' buttons, from the launcher's data. */
  drawTools() {
    if (!this.tools) return;
    const create = shellData?.create ?? [];
    // Panels that need something to show (a file's comments) open from apps, not from the bar.
    const panels = (shellData?.sidepanels ?? []).filter((p) => !p.contextual);
    this.createBtn = create.length
      ? el("button", { type: "button", "aria-label": "Create", "aria-haspopup": "menu", "aria-expanded": "false", title: "Create", onclick: () => this.toggleCreate() })
      : null;
    if (this.createBtn) this.createBtn.innerHTML = PLUS;
    const open = sidepanelOpen();
    this.tools.replaceChildren(
      ...[this.createBtn].filter(Boolean),
      ...panels.map((p) =>
        el("button", {
          type: "button",
          "aria-pressed": open === p.id ? "true" : "false",
          "aria-label": p.title,
          title: p.title,
          "data-panel": p.id,
          text: (p.title ?? "?").slice(0, 1).toUpperCase(),
          onclick: () => (sidepanelOpen() === p.id ? closeSidepanel() : openSidepanel(p.id)),
        }),
      ),
    );
  }

  toggleCreate() {
    const open = this.createMenu.hidden;
    this.closePanels(false);
    if (!open || !this.createBtn) return;
    this.createMenu.replaceChildren(
      ...(shellData?.create ?? []).map((c) => el("a", { role: "menuitem", href: c.url, text: c.label, onclick: (e) => (e.preventDefault(), navigate(c.url)) })),
    );
    this.createMenu.hidden = false;
    this.createBtn.setAttribute("aria-expanded", "true");
    this.createMenu.querySelector("a")?.focus();
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
      const found = (shellData?.apps ?? []).filter((a) => !q || `${a.title} ${a.description ?? ""}`.toLowerCase().includes(q));
      // A family's entries sit together under its name (§11.5), where its first one would be.
      const apps = [];
      for (const a of found) {
        if (apps.includes(a)) continue;
        // The head of the family first (Drive before Docs), each app's entries in their order.
        const members = a.family ? found.filter((b) => b.family === a.family).sort((x, y) => Number(x.app !== x.family) - Number(y.app !== y.family)) : [a];
        const fam = a.family && (shellData?.families ?? []).find((f) => f.id === a.family);
        if (fam && members.length > 1) apps.push({ group: fam.title });
        apps.push(...members);
      }
      grid.replaceChildren(
        ...apps.map((a) => {
          if (a.group) return el("p", { class: "group", role: "presentation", text: a.group });
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
      if (!found.length) grid.append(el("p", { class: "empty", text: shellData ? (q ? "No app matches." : "No apps yet.") : "Loading…" }));
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
      el("a", { role: "menuitem", href: "/account/", text: "Account and devices" }),
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

// When the page goes away for another page of the suite (a link, `navigate`), uploads carry on in
// the next page; only closing the last tab asks first (§12.6).
let leavingInSuite = 0;
document.addEventListener(
  "click",
  (e) => {
    const a = e.composedPath().find((n) => n instanceof HTMLAnchorElement);
    if (a?.href && new URL(a.href, location.href).origin === location.origin && !a.target && !a.download) leavingInSuite = Date.now();
  },
  true,
);

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
  return guardedLeave(() => {
    leavingInSuite = Date.now();
    location.assign(url);
  });
}

// ───────────────────────── intents (§11.3) ─────────────────────────

async function handlersFor(action, type) {
  const data = shellData ?? (await loadShell());
  return (data?.intents ?? []).filter((i) => i.action === action && i.type === type);
}

/** The app a person picked for an intent ("always use"), if it still handles it. */
function remembered(action, type, handlers) {
  const app = memory.get(localStorage, `ferrum.intent.${action}.${type}`);
  return handlers.find((h) => h.app === app) ?? null;
}

function appTitle_(app) {
  return (shellData?.apps ?? []).find((a) => a.app === app)?.title ?? app;
}

/** Asks which app should do it when several can; null when the person closes the question. */
function choose(action, type, handlers) {
  return new Promise((resolve) => {
    const dialog = document.createElement("dialog");
    Object.assign(dialog.style, {
      padding: "16px",
      border: "1px solid var(--ferrum-border, #d9dee5)",
      borderRadius: "10px",
      background: "var(--ferrum-surface, #fff)",
      color: "var(--ferrum-text, #17191d)",
      boxShadow: "var(--ferrum-shadow)",
      font: "14px/1.4 var(--ferrum-font, system-ui, sans-serif)",
      minWidth: "280px",
    });
    dialog.setAttribute("aria-labelledby", "ferrum-choose-title");
    const always = el("input", { type: "checkbox", id: "ferrum-choose-always" });
    let picked = null;
    const buttons = handlers.map((h) =>
      el("button", {
        type: "button",
        style: "display:block;width:100%;text-align:left;margin:4px 0;padding:8px 10px;border:1px solid var(--ferrum-border,#d9dee5);border-radius:6px;background:transparent;color:inherit;font:inherit;cursor:pointer",
        text: h.title ? `${appTitle_(h.app)} · ${h.title}` : appTitle_(h.app),
        onclick: () => {
          picked = h;
          dialog.close();
        },
      }),
    );
    dialog.append(
      el("h2", { id: "ferrum-choose-title", style: "margin:0 0 8px;font-size:15px", text: "Which app should do this?" }),
      ...buttons,
      el("label", { style: "display:flex;gap:6px;align-items:center;margin-top:10px" }, always, el("span", { text: "Always use this app" })),
    );
    dialog.addEventListener("close", () => {
      if (picked && always.checked) memory.set(localStorage, `ferrum.intent.${action}.${type}`, picked.app);
      dialog.remove();
      resolve(picked);
    });
    document.body.append(dialog);
    dialog.showModal();
  });
}

function intent({ action, type, data } = {}) {
  return new Promise((resolve, reject) => {
    handlersFor(action, type).then(async (handlers) => {
      if (!handlers.length) {
        const err = new Error(`no app handles ${action} ${type}`);
        err.code = "no_handler";
        reject(err);
        return;
      }
      const h = handlers.length === 1 ? handlers[0] : (remembered(action, type, handlers) ?? (await choose(action, type, handlers)));
      if (!h) {
        resolve({ status: "cancelled" });
        return;
      }
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

// ───────────────────────── side panels (§11.4) ─────────────────────────

// One panel at a time, beside the page: another app's page in a same-origin frame (its runtime
// knows it is a panel from `_panel`). It opens again on the next page of this tab, and its page
// restores its own state from the server.
const PANEL_KEY = "ferrum.sidepanel";
const PANEL_MIN = 300;
const PANEL_MAX = 720;
let panelFrame = null;
let panelContext = null;
// The panel open beside this page. Panels from the bar open again on the tab's next page; a
// contextual one (a file's comments) belongs to the page that opened it.
let openPanel = null;

function sidepanelOpen() {
  return openPanel?.id ?? null;
}

function rememberedPanel() {
  return memory.get(sessionStorage, PANEL_KEY)?.id ?? null;
}

function panelWidth() {
  const w = Number(memory.get(localStorage, "ferrum.sidepanel.width"));
  return Math.min(PANEL_MAX, Math.max(PANEL_MIN, Number.isFinite(w) && w > 0 ? w : 380), Math.max(PANEL_MIN, innerWidth - 320));
}

function layoutPanel(open) {
  const root = document.documentElement;
  if (open) {
    root.dataset.ferrumPanel = open;
    root.style.setProperty("--ferrum-panel-width", `${panelWidth()}px`);
  } else {
    delete root.dataset.ferrumPanel;
    root.style.removeProperty("--ferrum-panel-width");
  }
}

async function openSidepanel(id, context) {
  const b = bar();
  if (!b?.side) return false;
  const data = shellData ?? (await loadShell());
  const p = (data?.sidepanels ?? []).find((x) => x.id === id || `${x.app}.${x.id}` === id);
  if (!p) return false;
  // A phone has no room beside the page: the panel's app instead.
  if (matchMedia("(max-width: 640px)").matches) {
    const entry = (data?.apps ?? []).find((a) => a.app === p.app);
    if (entry) navigate(entry.url);
    return Boolean(entry);
  }
  const ctx = context === undefined || context === null ? null : JSON.stringify(context);
  if (openPanel?.id === p.id && openPanel.app === p.app && panelFrame) {
    if (ctx) panelFrame.contentWindow?.postMessage({ type: "ferrum:panel-context", context: JSON.parse(ctx) }, location.origin);
    return true;
  }
  const url = new URL(p.url, location.origin);
  url.searchParams.set("_panel", p.id);
  if (ctx && ctx.length <= 2000) url.searchParams.set("_ctx", ctx);
  openPanel = p;
  if (p.contextual) {
    try {
      sessionStorage.removeItem(PANEL_KEY);
    } catch {}
  } else memory.set(sessionStorage, PANEL_KEY, { id: p.id });
  drawSidepanel(p, url.pathname + url.search);
  return true;
}

function drawSidepanel(p, src) {
  const b = bar();
  panelFrame = el("iframe", { title: p.title, src });
  const open = el("button", { type: "button", "aria-label": `Open ${p.title}`, title: `Open ${p.title}`, onclick: () => {
    const entry = (shellData?.apps ?? []).find((a) => a.app === p.app);
    if (entry) navigate(entry.url);
  } });
  open.innerHTML = OPEN;
  const close = el("button", { type: "button", "aria-label": `Close ${p.title}`, title: "Close", onclick: () => closeSidepanel() });
  close.innerHTML = CLOSE;
  const grip = el("div", { class: "grip", role: "separator", "aria-orientation": "vertical", "aria-label": "Resize the side panel", tabindex: "0" });
  const resize = (w) => {
    const width = Math.round(Math.min(PANEL_MAX, Math.max(PANEL_MIN, w)));
    memory.set(localStorage, "ferrum.sidepanel.width", width);
    document.documentElement.style.setProperty("--ferrum-panel-width", `${width}px`);
  };
  grip.addEventListener("pointerdown", (e) => {
    grip.setPointerCapture(e.pointerId);
    // The frame would swallow the moves while the pointer is over it.
    panelFrame.style.pointerEvents = "none";
    const move = (m) => resize(innerWidth - m.clientX);
    const up = () => {
      panelFrame.style.pointerEvents = "";
      grip.removeEventListener("pointermove", move);
    };
    grip.addEventListener("pointermove", move);
    grip.addEventListener("pointerup", up, { once: true });
    grip.addEventListener("pointercancel", up, { once: true });
  });
  grip.addEventListener("keydown", (e) => {
    if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
      e.preventDefault();
      resize(panelWidth() + (e.key === "ArrowLeft" ? 24 : -24));
    }
  });
  b.side.replaceChildren(grip, el("header", {}, el("strong", { text: p.title }), open, close), panelFrame);
  b.side.hidden = false;
  layoutPanel(p.id);
  b.drawTools();
  events.dispatchEvent(new CustomEvent("sidepanel", { detail: p.id }));
}

function closeSidepanel() {
  const b = bar();
  try {
    sessionStorage.removeItem(PANEL_KEY);
  } catch {}
  openPanel = null;
  panelFrame = null;
  if (b?.side) {
    b.side.hidden = true;
    b.side.replaceChildren();
  }
  layoutPanel(null);
  b?.drawTools();
  events.dispatchEvent(new CustomEvent("sidepanel", { detail: null }));
}

window.addEventListener("message", (e) => {
  if (!panelFrame || e.source !== panelFrame.contentWindow || e.origin !== location.origin) return;
  if (e.data?.type === "ferrum:navigate" && typeof e.data.url === "string" && e.data.url.startsWith("/") && !e.data.url.startsWith("//")) navigate(e.data.url);
  else if (e.data?.type === "ferrum:panel-close") closeSidepanel();
});

if (panelId) {
  try {
    panelContext = JSON.parse(params.get("_ctx") ?? "null");
  } catch {
    panelContext = null;
  }
  window.addEventListener("message", (e) => {
    if (e.source !== window.parent || e.origin !== location.origin || e.data?.type !== "ferrum:panel-context") return;
    panelContext = e.data.context ?? null;
    events.dispatchEvent(new CustomEvent("panel-context", { detail: panelContext }));
  });
}

const sidepanel = Object.freeze({
  /** Opens a side panel (`{id, context}` or an id) beside this page. */
  open: (o) => {
    const { id, context } = typeof o === "string" ? { id: o } : (o ?? {});
    if (inFrame || hostKind !== "page") return Promise.resolve(false);
    return openSidepanel(id, context);
  },
  close: () => (panelId ? window.parent.postMessage({ type: "ferrum:panel-close" }, location.origin) : closeSidepanel()),
  /** The panel open beside this page, or this page's own id when it is a panel. */
  current: () => panelId ?? sidepanelOpen(),
  /** What the panel was opened with (`context`), in the panel's page. */
  context: () => panelContext,
  list: () => loadShell().then((d) => d?.sidepanels ?? []),
});

// ───────────────────────── families (§11.5) ─────────────────────────

/** This app's family: its name and the entries of its members the person may use. */
async function family() {
  const data = shellData ?? (await loadShell());
  const head = (data?.apps ?? []).find((a) => a.app === boot?.app?.id && a.family)?.family;
  const fam = head && (data?.families ?? []).find((f) => f.id === head);
  if (!fam) return null;
  const entries = (data.apps ?? [])
    .filter((a) => a.family === head)
    .sort((x, y) => Number(x.app !== head) - Number(y.app !== head))
    .map((a) => ({ id: a.id, app: a.app, title: a.title, url: a.url, icon: a.icon ?? null, color: a.color ?? null, current: location.pathname.startsWith(a.url) }));
  return { id: fam.id, title: fam.title, entries };
}

// ───────────────────────── uploads that outlive the page (§12.6) ─────────────────────────

// An app hands a file to the runtime with the request that stores it (one of this site's /api/
// addresses). The file waits in IndexedDB until it is sent, so going to another app's page is
// fine: that page's runtime goes on with it. One tab sends each file (a Web Lock per upload); the
// request carries the upload's id as its Idempotency-Key, so a retry never stores a file twice
// where the app honours it. Uploads belong to the person and workspace that started them.
const FINISHED = new Set(["done", "failed", "cancelled"]);
const UPLOAD_PERSIST_MAX = 2 * 1024 ** 3;
const UPLOAD_EXPIRY = 7 * 86400e3;
const UPLOAD_KEEP_DONE = 3600e3;
const UPLOAD_TRIES = 6;
const uploadsChannel = "BroadcastChannel" in window ? new BroadcastChannel("ferrum-uploads") : null;
const uploads = new Map();
const memoryFiles = new Map();
const uploadWaiters = new Map();
const running = new Map();
let uploadsDb = null;

const owner = () => `${boot?.workspace?.id ?? ""}/${boot?.person?.id ?? ""}`;
const mine = (u) => u.owner === owner();

function idb() {
  uploadsDb ??= new Promise((resolve) => {
    let req;
    try {
      req = indexedDB.open("ferrum-uploads", 1);
    } catch {
      resolve(null);
      return;
    }
    req.onupgradeneeded = () => req.result.createObjectStore("items", { keyPath: "id" });
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => resolve(null);
    req.onblocked = () => resolve(null);
  });
  return uploadsDb;
}

async function idbDo(mode, fn) {
  const db = await idb();
  if (!db) return undefined;
  return new Promise((resolve, reject) => {
    let out;
    const tx = db.transaction("items", mode);
    const r = fn(tx.objectStore("items"));
    if (r) r.onsuccess = () => (out = r.result);
    tx.oncomplete = () => resolve(out);
    tx.onerror = tx.onabort = () => reject(tx.error);
  });
}

const idbPut = (item) => idbDo("readwrite", (s) => s.put(item)).catch(() => undefined);
const idbGet = (id) => idbDo("readonly", (s) => s.get(id)).catch(() => undefined);
const idbAll = () => idbDo("readonly", (s) => s.getAll()).then((v) => v ?? [], () => []);
const idbDelete = (id) => idbDo("readwrite", (s) => s.delete(id)).catch(() => undefined);

/** What a page may know of an upload: everything but the file. */
const brief = ({ file: _file, ...rest }) => rest;

function changed(item, local = true) {
  uploads.set(item.id, item);
  if (local) uploadsChannel?.postMessage({ type: "upload", item });
  drawUploads();
  if (item.state === "done" || item.state === "failed" || item.state === "cancelled") {
    events.dispatchEvent(new CustomEvent(`upload-${item.state}`, { detail: item }));
    const w = uploadWaiters.get(item.id);
    if (w) {
      uploadWaiters.delete(item.id);
      if (item.state === "done") w.resolve(item.result);
      else w.reject(Object.assign(new Error(item.error ?? "The upload did not finish."), { code: item.state }));
    }
  } else {
    events.dispatchEvent(new CustomEvent("upload-progress", { detail: item }));
  }
}

async function save(item, patch) {
  const next = { ...item, ...patch, updated: Date.now() };
  // The file stays in the database until the upload is over.
  const rec = await idbGet(item.id);
  if (rec) await idbPut(FINISHED.has(next.state) ? brief(next) : { ...next, file: rec.file });
  changed(brief(next));
  return next;
}

function failure(status, text) {
  let body = null;
  try {
    body = JSON.parse(text);
  } catch {}
  const message = body?.message ?? (status ? `The server refused the file (${status}).` : "Cannot reach the server.");
  return Object.assign(new Error(message), { status, retry: status === 0 || status === 408 || status === 429 || status >= 500, signedOut: status === 401 });
}

function send(item, file) {
  return new Promise((resolve, reject) => {
    const x = new XMLHttpRequest();
    x.open(item.method, item.url);
    for (const [k, v] of Object.entries(item.headers ?? {})) x.setRequestHeader(k, v);
    x.setRequestHeader("idempotency-key", item.id);
    x.setRequestHeader("accept", "application/json");
    let last = 0;
    x.upload.onprogress = (e) => {
      if (Date.now() - last < 250 && e.loaded < e.total) return;
      last = Date.now();
      changed({ ...uploads.get(item.id), loaded: e.loaded, updated: Date.now() });
    };
    x.onload = () => {
      running.delete(item.id);
      watchUploadLeaving();
      if (x.status >= 200 && x.status < 300) {
        let result = null;
        try {
          result = x.responseText ? JSON.parse(x.responseText) : null;
        } catch {
          result = x.responseText;
        }
        resolve(result);
      } else reject(failure(x.status, x.responseText));
    };
    x.onerror = () => {
      running.delete(item.id);
      watchUploadLeaving();
      reject(failure(0, ""));
    };
    x.onabort = () => {
      running.delete(item.id);
      watchUploadLeaving();
      reject(Object.assign(new Error("Stopped."), { aborted: true }));
    };
    running.set(item.id, x);
    watchUploadLeaving();
    x.send(file);
  });
}

async function runUpload(id) {
  let item = (await idbGet(id)) ?? (uploads.get(id) ? { ...uploads.get(id), file: memoryFiles.get(id) } : null);
  if (!item || FINISHED.has(item.state) || !mine(item)) return;
  const file = item.file;
  if (Date.now() - item.created > UPLOAD_EXPIRY || !file) {
    await save(brief(item), { state: "failed", error: file ? "This upload waited too long and was dropped." : "The file is not here any more: upload it again." });
    return;
  }
  item = await save(brief(item), { state: "uploading", attempts: (item.attempts ?? 0) + 1, loaded: 0, error: null });
  try {
    const result = await send(item, file);
    memoryFiles.delete(id);
    await save(item, { state: "done", loaded: item.size, result });
  } catch (e) {
    if (e.aborted) {
      // Cancelled here (or this page is going): cancel() has recorded it, or the next page resumes.
      return;
    }
    if (e.signedOut) {
      await save(item, { state: "queued", error: "Waiting for you to sign in again." });
      return;
    }
    if (e.retry && item.attempts < UPLOAD_TRIES) {
      await save(item, { state: "queued", error: e.message });
      setTimeout(pumpUploads, Math.min(60e3, 1000 * 2 ** item.attempts));
      return;
    }
    memoryFiles.delete(id);
    await save(item, { state: "failed", error: e.message });
  }
}

let pumping = false;
async function pumpUploads() {
  if (pumping || hostKind === "public" || !boot?.person) return;
  pumping = true;
  try {
    for (const rec of await idbAll()) {
      if (!mine(rec)) {
        // Someone else's, long forgotten (a shared device): gone.
        if (Date.now() - rec.updated > UPLOAD_EXPIRY) await idbDelete(rec.id);
        continue;
      }
      if (FINISHED.has(rec.state)) {
        if (Date.now() - rec.updated > (rec.state === "done" ? UPLOAD_KEEP_DONE : UPLOAD_EXPIRY)) {
          await idbDelete(rec.id);
          uploads.delete(rec.id);
        } else if (!uploads.has(rec.id)) uploads.set(rec.id, brief(rec));
        continue;
      }
      if (!uploads.has(rec.id)) uploads.set(rec.id, brief(rec));
    }
    for (const u of [...uploads.values()].filter((u) => mine(u) && u.state === "queued")) {
      const go = () => runUpload(u.id);
      if (navigator.locks) await navigator.locks.request(`ferrum-upload:${u.id}`, { ifAvailable: true }, (lock) => (lock ? go() : undefined));
      else await go();
    }
    // Uploads another page left half-sent: their lock is free again (without Web Locks, once
    // nothing has moved for a minute).
    for (const u of [...uploads.values()].filter((u) => mine(u) && u.state === "uploading" && !running.has(u.id))) {
      if (navigator.locks) await navigator.locks.request(`ferrum-upload:${u.id}`, { ifAvailable: true }, (lock) => (lock ? runUpload(u.id) : undefined));
      else if (Date.now() - u.updated > 60e3) await runUpload(u.id);
    }
  } finally {
    pumping = false;
    drawUploads();
  }
}

async function startUpload({ file, url, method = "POST", headers = {}, label } = {}) {
  if (!(file instanceof Blob)) throw new TypeError("uploads.start: {file, url}");
  if (typeof url !== "string" || !url.startsWith("/api/") || url.includes("..")) throw new TypeError("uploads.start: url must be one of this site's /api/ addresses");
  if (!["POST", "PUT"].includes(method)) throw new TypeError("uploads.start: method is POST or PUT");
  if (!boot?.person) throw new Error("uploads need a signed-in person");
  const plain = {};
  for (const [k, v] of Object.entries(headers)) if (/^[a-z0-9-]{1,60}$/i.test(k) && typeof v === "string" && !/[\r\n]/.test(v)) plain[k.toLowerCase()] = v;
  const id = crypto.randomUUID();
  const item = {
    id,
    owner: owner(),
    app: boot?.app?.id ?? null,
    label: String(label ?? file.name ?? "File").slice(0, 200),
    name: file.name ?? "file",
    size: file.size,
    type: file.type,
    url,
    method,
    headers: plain,
    state: "queued",
    loaded: 0,
    attempts: 0,
    created: Date.now(),
    updated: Date.now(),
    error: null,
    result: null,
  };
  // Kept in the browser until sent, so it survives this page; very large files stay in memory
  // (they then go only while this page lives).
  const kept = file.size <= UPLOAD_PERSIST_MAX && (await idbPut({ ...item, file })) !== undefined;
  if (!kept) memoryFiles.set(id, file);
  const done = new Promise((resolve, reject) => uploadWaiters.set(id, { resolve, reject }));
  done.catch(() => {});
  changed({ ...item, kept });
  pumpUploads();
  return { id, done };
}

async function cancelUpload(id) {
  const u = uploads.get(id);
  if (!u || FINISHED.has(u.state)) return;
  running.get(id)?.abort();
  uploadsChannel?.postMessage({ type: "cancel", id });
  memoryFiles.delete(id);
  await save(u, { state: "cancelled", error: null });
}

async function retryUpload(id) {
  const rec = await idbGet(id);
  if (!rec || rec.state !== "failed" || !rec.file) return false;
  await idbPut({ ...rec, state: "queued", attempts: 0, created: Date.now(), updated: Date.now(), error: null });
  uploads.set(id, brief({ ...rec, state: "queued", attempts: 0 }));
  pumpUploads();
  return true;
}

async function dismissUpload(id) {
  const u = uploads.get(id);
  if (u && !FINISHED.has(u.state)) return;
  uploads.delete(id);
  await idbDelete(id);
  uploadsChannel?.postMessage({ type: "dismiss", id });
  drawUploads();
}

/** On sign-out: this person's uploads go, with their files. */
async function forgetUploads() {
  for (const u of [...uploads.values()].filter(mine)) {
    running.get(u.id)?.abort();
    uploads.delete(u.id);
    await idbDelete(u.id);
  }
  for (const rec of await idbAll()) if (mine(rec)) await idbDelete(rec.id);
}

uploadsChannel?.addEventListener("message", (e) => {
  const m = e.data ?? {};
  if (m.type === "upload" && m.item?.id && mine(m.item)) changed(m.item, false);
  else if (m.type === "cancel" && running.has(m.id)) running.get(m.id).abort();
  else if (m.type === "dismiss") {
    uploads.delete(m.id);
    drawUploads();
  }
});

const sizeText = (n) => (n >= 1024 ** 3 ? `${(n / 1024 ** 3).toFixed(1)} GB` : n >= 1024 ** 2 ? `${(n / 1024 ** 2).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`);

/** The bar's upload indicator and its tray. */
function drawUploads() {
  const b = bar();
  if (!b?.tray) return;
  const list = [...uploads.values()].filter(mine).sort((x, y) => x.created - y.created);
  const active = list.filter((u) => !FINISHED.has(u.state));
  const failed = list.filter((u) => u.state === "failed");
  let text = "";
  if (active.length) {
    const total = active.reduce((n, u) => n + u.size, 0) || 1;
    const sent = active.reduce((n, u) => n + (u.loaded ?? 0), 0);
    text = `${active.length === 1 ? "Uploading 1 file" : `Uploading ${active.length} files`} · ${Math.floor((sent / total) * 100)}%`;
  } else if (failed.length) text = failed.length === 1 ? "1 upload failed" : `${failed.length} uploads failed`;
  else if (list.some((u) => u.state === "done")) text = "Uploads done";
  b.setIndicator("uploads", text, () => toggleTray());
  if (!b.tray.hidden) fillTray();
}

function toggleTray() {
  const b = bar();
  const open = b.tray.hidden;
  b.closePanels(false);
  if (!open) return;
  fillTray();
  b.tray.hidden = false;
  b.tray.querySelector("button")?.focus();
}

function fillTray() {
  const b = bar();
  const list = [...uploads.values()].filter(mine).sort((x, y) => x.created - y.created);
  const rows = list.map((u) => {
    const state =
      u.state === "done" ? "Done" : u.state === "failed" ? "Failed" : u.state === "cancelled" ? "Stopped" : u.state === "queued" ? (u.error ?? "Waiting") : `${sizeText(u.loaded ?? 0)} of ${sizeText(u.size)}`;
    const actions = FINISHED.has(u.state)
      ? [
          u.state === "failed" && u.kept !== false ? el("button", { type: "button", text: "Retry", onclick: () => retryUpload(u.id) }) : null,
          el("button", { type: "button", "aria-label": `Dismiss ${u.label}`, text: "Dismiss", onclick: () => dismissUpload(u.id) }),
        ]
      : [el("button", { type: "button", "aria-label": `Stop uploading ${u.label}`, text: "Stop", onclick: () => cancelUpload(u.id) })];
    return el(
      "li",
      {},
      el("div", { class: "row" }, el("span", { class: "name", title: u.label, text: u.label }), ...actions.filter(Boolean)),
      FINISHED.has(u.state) ? null : el("progress", { max: String(u.size || 1), value: String(u.loaded ?? 0), "aria-label": `${u.label} sent` }),
      el("small", { class: u.state === "failed" ? "err" : null, text: u.state === "failed" && u.error ? `${state}: ${u.error}` : state }),
    );
  });
  const finished = list.filter((u) => FINISHED.has(u.state));
  b.tray.replaceChildren(
    rows.length ? el("ul", {}, ...rows) : el("p", { class: "empty", text: "No uploads." }),
    finished.length ? el("p", { class: "actions" }, el("button", { type: "button", text: "Clear finished", onclick: () => finished.forEach((u) => dismissUpload(u.id)) })) : null,
  );
}

// The other tabs of this site, so closing one while another can carry on asks nothing.
const tabsChannel = "BroadcastChannel" in window ? new BroadcastChannel("ferrum-tabs") : null;
const tabId = crypto.randomUUID();
const otherTabs = new Map();
tabsChannel?.addEventListener("message", (e) => {
  const { type, id } = e.data ?? {};
  if (!id || id === tabId) return;
  if (type === "bye") otherTabs.delete(id);
  else otherTabs.set(id, Date.now());
  if (type === "hello") tabsChannel.postMessage({ type: "here", id: tabId });
});
tabsChannel?.postMessage({ type: "hello", id: tabId });
setInterval(() => tabsChannel?.postMessage({ type: "here", id: tabId }), 10e3);
window.addEventListener("pagehide", (e) => {
  if (!e.persisted) tabsChannel?.postMessage({ type: "bye", id: tabId });
});
window.addEventListener("pageshow", (e) => {
  if (e.persisted) tabsChannel?.postMessage({ type: "hello", id: tabId });
});

// Closing the last tab while a file is being sent asks first; moving to another page does not.
// Listened for only while this page sends something: at other times the listener would keep
// pages out of Firefox's back/forward cache.
const onUploadLeave = (e) => {
  if (!running.size || Date.now() - leavingInSuite < 3000) return;
  const others = [...otherTabs.values()].some((t) => Date.now() - t < 25e3);
  if (!others) e.preventDefault();
};
function watchUploadLeaving() {
  if (running.size) window.addEventListener("beforeunload", onUploadLeave);
  else window.removeEventListener("beforeunload", onUploadLeave);
}

const uploadsApi = Object.freeze({
  start: startUpload,
  cancel: cancelUpload,
  retry: retryUpload,
  dismiss: dismissUpload,
  list: () => [...uploads.values()].filter(mine),
  on: (event, cb) => {
    const f = (e) => cb(e.detail);
    events.addEventListener(`upload-${event}`, f);
    return () => events.removeEventListener(`upload-${event}`, f);
  },
});

if (boot?.person && hostKind !== "public") {
  (window.requestIdleCallback ?? ((f) => setTimeout(f, 800)))(() => pumpUploads());
  window.addEventListener("online", () => pumpUploads());
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") pumpUploads();
  });
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
    if (e.data?.type === "state") {
      if (e.data.state === "open") liveWasOpen = true;
      liveState = e.data.state;
    }
  };
  // A page leaving for good says so; a page put in the back/forward cache comes back with pageshow.
  window.addEventListener("pagehide", (e) => {
    if (!e.persisted) livePort?.postMessage({ type: "bye" });
  });
  return livePort;
}

let liveState = "closed";
let liveWasOpen = false;

/** Subscribes to topics; `resync` runs when the channel comes back after a break (changes made
 *  meanwhile were missed: refetch what is on screen). */
function subscribe(topics, onChange, { resync } = {}) {
  const list = (Array.isArray(topics) ? topics : [topics]).filter((t) => typeof t === "string");
  const port = livePortOf();
  const handler = (msg) => {
    if (msg?.type === "state") {
      if (msg.state === "open" && liveState !== "open" && liveWasOpen) resync?.();
      return;
    }
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
  (hostKind === "page" || hostKind === "public") && boot?.app
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
  if (!boot?.app?.id || outdated || hostKind === "public") return;
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

// A public page (§7.3) has no person: no intents, panels, uploads or live channel.
const PUBLIC_CAPABILITIES = ["navigate", "title", "toast", "theme", "locale", "ready"];
const CAPABILITIES = new Set(
  hostKind === "public"
    ? PUBLIC_CAPABILITIES
    : [...PUBLIC_CAPABILITIES, "breadcrumbs", "badge", "intent", "canIntent", "companion", "leaveGuard", "session", "live", "sidepanel", "uploads", "family"],
);

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
  sidepanel,
  uploads: uploadsApi,
  family,
  apps: () => loadShell().then((d) => d?.apps ?? []),
  on: (event, cb) => {
    const f = (e) => cb(e.detail);
    events.addEventListener(event, f);
    return () => events.removeEventListener(event, f);
  },
});

const ns = globalThis.ferrum && typeof globalThis.ferrum === "object" ? globalThis.ferrum : {};
Object.defineProperty(globalThis, "ferrum", { value: Object.freeze({ ...ns, shell }), configurable: false, writable: false });
document.documentElement.dataset.ferrumHost = hostKind;

// Last: defining the element draws the bar at once (it is in the page already), and drawing uses
// everything above.
customElements.define("ferrum-bar", FerrumBar);
