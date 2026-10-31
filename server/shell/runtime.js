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

// ───────────────────────── appearance ─────────────────────────

// The person's appearance on this device, under the web app's key (web/src/lib/theme.ts): light,
// dark or the system's, contrast and density, on <html> before the app draws, so the theme's
// tokens, the kit's `dark:` styles and the bar agree on every app's page.
// ponytail: no accent colour here; nothing sets one yet (lib/theme.ts's accentTokens when something does).
const prefersDark = matchMedia("(prefers-color-scheme: dark)");
const prefersMore = matchMedia("(prefers-contrast: more)");
function applyAppearance() {
  const a = memory.get(localStorage, "ferrum.appearance") ?? {};
  const root = document.documentElement.dataset;
  root.theme = a.theme === "light" || a.theme === "dark" ? a.theme : prefersDark.matches ? "dark" : "light";
  root.contrast = a.contrast === "more" || (a.contrast !== "normal" && prefersMore.matches) ? "more" : "normal";
  root.density = a.density === "compact" ? "compact" : "comfortable";
}
applyAppearance();
prefersDark.addEventListener("change", applyAppearance);
prefersMore.addEventListener("change", applyAppearance);
window.addEventListener("storage", (e) => e.key === "ferrum.appearance" && applyAppearance());

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
:host { all: initial; color-scheme: inherit; display: block; position: sticky; top: 0; z-index: 2147483000; height: var(--ferrum-bar-height, 48px);
  font: 14px/1.4 var(--ferrum-font, system-ui, sans-serif); color: var(--ferrum-text, #17191d); -webkit-font-smoothing: antialiased; }
:host([hidden]), [hidden] { display: none !important; }
* { box-sizing: border-box; }
svg { flex: none; display: block; }
.bar { display: flex; align-items: center; gap: 4px; height: 100%; padding: 0 10px 0 6px; background: var(--ferrum-surface, #fff);
  border-bottom: 1px solid var(--ferrum-border, #d9dee5); }
button, a.btn { display: inline-flex; align-items: center; justify-content: center; gap: 6px; min-width: 34px; height: 34px; padding: 0 8px;
  border: 1px solid transparent; border-radius: 8px; background: transparent; color: inherit; font: inherit; text-decoration: none; cursor: pointer;
  transition: background-color .12s ease, color .12s ease; }
button:hover, a.btn:hover { background: var(--ferrum-surface-2, #eef1f5); }
button:focus-visible, a:focus-visible, input:focus-visible { outline: none; box-shadow: var(--ferrum-focus-ring, 0 0 0 3px rgb(47 91 234 / .35)); }
button[aria-pressed="true"], button[aria-expanded="true"] { background: var(--ferrum-primary-soft, #e7edfd); color: var(--ferrum-primary, #2f5bea); }
.launch { color: var(--ferrum-muted, #5b6472); }
.home { color: var(--ferrum-muted, #5b6472); text-decoration: none; padding: 4px 8px; border-radius: 6px; white-space: nowrap; overflow: hidden;
  text-overflow: ellipsis; max-width: 14em; font-weight: 500; }
.home:hover { color: var(--ferrum-text, #17191d); background: var(--ferrum-surface-2, #eef1f5); }
.sep { color: var(--ferrum-border, #d9dee5); font-size: 18px; line-height: 1; padding: 0 2px; }
.app { display: inline-flex; align-items: center; gap: 8px; min-width: 0; text-decoration: none; color: inherit; border-radius: 8px; padding: 3px 6px 3px 3px; }
.app:hover { background: var(--ferrum-surface-2, #eef1f5); }
.title { font-weight: 650; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; min-width: 0; letter-spacing: -.005em; }
.crumbs { color: var(--ferrum-muted, #5b6472); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; min-width: 0; padding-left: 4px; }
.spacer { flex: 1; }
.me { width: 32px; min-width: 32px; height: 32px; padding: 0; border-radius: 50%; background: var(--ferrum-primary-soft, #e7edfd);
  color: var(--ferrum-primary, #2f5bea); font-weight: 650; font-size: 12px; margin-left: 4px; }
.me:hover { filter: brightness(.97); background: var(--ferrum-primary-soft, #e7edfd); }
.tools, .indicators { display: inline-flex; align-items: center; gap: 2px; }
.indicator { height: 28px; padding: 0 10px; border-radius: 999px; background: var(--ferrum-warning-soft, #fff3d6); font-size: 12px; font-weight: 500; }
.panel { position: absolute; top: calc(var(--ferrum-bar-height, 48px) + 4px); background: var(--ferrum-surface, #fff);
  border: 1px solid var(--ferrum-border, #d9dee5); border-radius: 12px; box-shadow: var(--ferrum-shadow, 0 8px 28px rgb(16 24 40 / .14));
  padding: 8px; max-height: calc(100vh - 72px); overflow: auto; animation: pop .12s ease-out; }
@keyframes pop { from { opacity: 0; transform: translateY(-4px) scale(.99); } to { opacity: 1; transform: none; } }
@media (prefers-reduced-motion: reduce) { .panel { animation: none; } button, a.btn { transition: none; } }
.launcher { left: 8px; width: min(440px, calc(100vw - 16px)); padding: 12px; }
.menu { right: 8px; min-width: 248px; }
.find { position: relative; margin-bottom: 10px; }
.find svg { position: absolute; left: 10px; top: 50%; transform: translateY(-50%); color: var(--ferrum-muted, #5b6472); pointer-events: none; }
.find input { width: 100%; height: 36px; padding: 0 10px 0 34px; border: 1px solid var(--ferrum-border, #d9dee5); border-radius: 8px;
  background: var(--ferrum-surface, #fff); color: inherit; font: inherit; }
.apps { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 2px; }
.tile-link { display: grid; justify-items: center; gap: 6px; padding: 10px 4px 8px; border-radius: 10px; color: inherit; text-decoration: none;
  text-align: center; font-size: 12.5px; line-height: 1.25; }
.tile-link span:last-child { max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tile-link:hover, .tile-link:focus-visible { background: var(--ferrum-surface-2, #eef1f5); }
.tile-link[aria-current="page"] { background: var(--ferrum-primary-soft, #e7edfd); }
.tile { display: grid; place-items: center; width: 42px; height: 42px; border-radius: 11px; color: #fff; font-weight: 700; font-size: 17px;
  box-shadow: inset 0 0 0 1px rgb(255 255 255 / .12), 0 1px 2px rgb(16 24 40 / .12); }
.tile.small { width: 26px; height: 26px; border-radius: 7px; font-size: 12px; box-shadow: inset 0 0 0 1px rgb(255 255 255 / .12); }
.group { grid-column: 1 / -1; display: flex; align-items: center; gap: 8px; margin: 10px 6px 2px; font-size: 11.5px; font-weight: 650;
  letter-spacing: .04em; text-transform: uppercase; color: var(--ferrum-muted, #5b6472); }
.group::after { content: ""; flex: 1; height: 1px; background: var(--ferrum-border, #d9dee5); }
.empty { color: var(--ferrum-muted, #5b6472); padding: 10px 6px; }
.foot { display: flex; gap: 4px; justify-content: space-between; margin-top: 10px; padding-top: 8px; border-top: 1px solid var(--ferrum-border, #d9dee5); }
.foot a { display: inline-flex; align-items: center; gap: 6px; padding: 6px 8px; border-radius: 6px; color: var(--ferrum-muted, #5b6472);
  text-decoration: none; font-size: 13px; }
.foot a:hover { background: var(--ferrum-surface-2, #eef1f5); color: var(--ferrum-text, #17191d); }
.who { display: flex; gap: 10px; align-items: center; padding: 8px 8px 12px; border-bottom: 1px solid var(--ferrum-border, #d9dee5); margin-bottom: 6px; }
.who .me { cursor: default; margin: 0; width: 38px; height: 38px; font-size: 14px; }
.who div { display: grid; min-width: 0; }
.who strong, .who small { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.who small { color: var(--ferrum-muted, #5b6472); font-size: 12.5px; }
.menu a, .menu button { display: flex; width: 100%; justify-content: flex-start; gap: 10px; height: 36px; padding: 0 10px; color: inherit;
  text-decoration: none; border-radius: 8px; font-size: 13.5px; }
.menu a svg, .menu button svg { color: var(--ferrum-muted, #5b6472); }
.menu hr { border: 0; border-top: 1px solid var(--ferrum-border, #d9dee5); margin: 6px 0; }
.notice { position: absolute; top: calc(var(--ferrum-bar-height, 48px) + 10px); left: 50%; transform: translateX(-50%); display: flex; gap: 10px;
  align-items: center; max-width: calc(100vw - 16px); padding: 8px 8px 8px 14px; border-radius: 10px; background: var(--ferrum-text, #17191d);
  color: var(--ferrum-surface, #fff); box-shadow: var(--ferrum-shadow); font-size: 13.5px; animation: pop .14s ease-out; }
.notice button { color: inherit; border-color: color-mix(in srgb, currentColor 35%, transparent); height: 28px; font-size: 13px; }
.notice button:hover { background: color-mix(in srgb, currentColor 12%, transparent); }
.skip { position: absolute; left: -9999px; }
.skip:focus { left: 8px; top: 8px; z-index: 1; padding: 6px 10px; background: var(--ferrum-surface, #fff); border-radius: 6px; }
.banner { position: absolute; top: var(--ferrum-bar-height, 48px); left: 0; right: 0; padding: 4px 12px; text-align: center; font-size: 12px;
  background: var(--ferrum-warning-soft, #fff3d6); }
.group-title { font-weight: 600; }
.create { right: 8px; min-width: 220px; }
.search { flex: 0 1 340px; justify-content: flex-start; gap: 8px; height: 34px; padding: 0 8px 0 10px; margin: 0 10px; border: 1px solid var(--ferrum-border, #d9dee5);
  border-radius: 9px; background: var(--ferrum-surface-2, #eef1f5); color: var(--ferrum-muted, #5b6472); font-size: 13.5px; min-width: 0; }
.search:hover { background: var(--ferrum-surface, #fff); border-color: color-mix(in srgb, var(--ferrum-primary, #2f5bea) 35%, var(--ferrum-border, #d9dee5)); }
.search kbd { margin-left: auto; padding: 1px 6px; border: 1px solid var(--ferrum-border, #d9dee5); border-radius: 5px; background: var(--ferrum-surface, #fff);
  font: 11px/1.5 var(--ferrum-font, system-ui, sans-serif); color: var(--ferrum-muted, #5b6472); }
.searchbox { left: 50%; transform: translateX(-50%); width: min(640px, calc(100vw - 16px)); padding: 10px; }
.results { max-height: min(60vh, 520px); overflow: auto; display: grid; gap: 1px; }
.result { display: flex; align-items: center; gap: 10px; padding: 6px 8px; border-radius: 8px; color: inherit; text-decoration: none; cursor: pointer; }
.result[aria-selected="true"] { background: var(--ferrum-primary-soft, #e7edfd); }
.result .glyph { display: grid; place-items: center; width: 26px; height: 26px; border-radius: 7px; background: var(--ferrum-surface-2, #eef1f5); color: var(--ferrum-muted, #5b6472); flex: none; }
.result .text { display: grid; min-width: 0; }
.result .text strong { font-weight: 550; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.result .text small { color: var(--ferrum-muted, #5b6472); font-size: 12px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.result mark { background: color-mix(in srgb, var(--ferrum-primary, #2f5bea) 22%, transparent); color: inherit; border-radius: 2px; }
.rgroup { display: flex; align-items: center; gap: 6px; margin: 10px 8px 4px; font-size: 11.5px; font-weight: 650; letter-spacing: .04em; text-transform: uppercase;
  color: var(--ferrum-muted, #5b6472); }
.rgroup:first-child { margin-top: 2px; }
.rgroup svg { animation: spin .9s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
.rhint { padding: 16px 8px; color: var(--ferrum-muted, #5b6472); text-align: center; }
.tray { right: 8px; width: min(380px, calc(100vw - 16px)); }
.tray h2 { margin: 2px 6px 10px; font-size: 13px; font-weight: 650; }
.tray ul { list-style: none; margin: 0; padding: 0; display: grid; gap: 4px; }
.tray li { display: grid; gap: 4px; padding: 8px 6px; border-radius: 8px; }
.tray li + li { border-top: 1px solid var(--ferrum-border, #d9dee5); }
.tray .row { display: flex; gap: 8px; align-items: center; }
.tray .row button { height: 28px; font-size: 12.5px; padding: 0 8px; border-color: var(--ferrum-border, #d9dee5); }
.tray .name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-weight: 500; }
.tray small { color: var(--ferrum-muted, #5b6472); font-size: 12px; }
.tray .err { color: var(--ferrum-danger, #c62828); }
.tray .ok { color: var(--ferrum-primary, #2f5bea); }
.tray .mark { color: var(--ferrum-muted, #5b6472); }
.tray progress { width: 100%; height: 4px; appearance: none; border: 0; border-radius: 99px; overflow: hidden; background: var(--ferrum-surface-2, #eef1f5); }
.tray progress::-webkit-progress-bar { background: var(--ferrum-surface-2, #eef1f5); }
.tray progress::-webkit-progress-value { background: var(--ferrum-primary, #2f5bea); border-radius: 99px; }
.tray progress::-moz-progress-bar { background: var(--ferrum-primary, #2f5bea); }
.tray .actions { display: flex; justify-content: flex-end; margin-top: 6px; }
.bell { position: relative; color: var(--ferrum-muted, #5b6472); }
.bell .badge { position: absolute; top: 2px; right: 1px; min-width: 16px; height: 16px; padding: 0 4px; border-radius: 999px; display: grid; place-items: center;
  background: var(--ferrum-danger, #d92d20); color: #fff; font: 600 10px/1 var(--ferrum-font, system-ui, sans-serif); box-shadow: 0 0 0 2px var(--ferrum-surface, #fff); }
.inbox { right: 8px; width: min(400px, calc(100vw - 16px)); padding: 6px; }
.ihead { display: flex; align-items: center; gap: 2px; padding: 2px 2px 6px 8px; }
.ihead h2 { flex: 1; margin: 0; font-size: 14px; font-weight: 650; }
.ihead .ghost { width: 32px; min-width: 32px; height: 32px; padding: 0; color: var(--ferrum-muted, #5b6472); }
.ihead .ghost:first-child { margin-left: -6px; }
.ihead .ghost[disabled] { opacity: .45; cursor: default; background: transparent; }
.istatus { margin: 0 8px; color: var(--ferrum-danger, #c62828); font-size: 12.5px; }
.istatus:empty { display: none; }
.ilist { list-style: none; margin: 0; padding: 0; display: grid; gap: 1px; max-height: min(70vh, 560px); overflow: auto; }
.note { display: flex; width: 100%; align-items: flex-start; justify-content: flex-start; gap: 10px; height: auto; padding: 9px 10px 9px 8px; border-radius: 8px;
  color: inherit; text-decoration: none; text-align: left; font-weight: 400; }
.note:hover { background: var(--ferrum-surface-2, #eef1f5); }
.note .dot { flex: none; width: 8px; height: 8px; margin-top: 6px; border-radius: 50%; }
.note.unread .dot { background: var(--ferrum-primary, #2f5bea); }
.note .text { display: grid; gap: 2px; min-width: 0; }
.note strong { font-weight: 500; font-size: 13.5px; line-height: 1.35; overflow-wrap: anywhere; }
.note.unread strong { font-weight: 650; }
.note .body { color: var(--ferrum-muted, #5b6472); font-size: 12.5px; display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; overflow-wrap: anywhere; }
.note .when { color: var(--ferrum-muted, #5b6472); font-size: 11.5px; }
.caught { display: grid; justify-items: center; gap: 4px; padding: 28px 16px; text-align: center; color: var(--ferrum-muted, #5b6472); }
.caught svg { color: var(--ferrum-primary, #2f5bea); margin-bottom: 4px; }
.caught strong { color: var(--ferrum-text, #17191d); font-weight: 600; }
.caught small { font-size: 12.5px; max-width: 30ch; }
.link { height: auto; min-width: 0; padding: 0; color: var(--ferrum-primary, #2f5bea); text-decoration: underline; }
.link:hover { background: transparent; }
.kinds { display: grid; gap: 2px; padding: 0 2px 4px; max-height: min(70vh, 560px); overflow: auto; }
.kinds .rgroup { margin: 10px 6px 2px; }
.kind { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 6px 6px; border-radius: 8px; }
.kind .what { display: grid; gap: 1px; min-width: 0; }
.kind label { font-size: 13.5px; }
.kind small { color: var(--ferrum-muted, #5b6472); font-size: 11.5px; }
.kind small:empty { display: none; }
.kind select { flex: none; height: 30px; max-width: 52%; padding: 0 6px; border: 1px solid var(--ferrum-border, #d9dee5); border-radius: 7px;
  background: var(--ferrum-surface, #fff); color: inherit; font: inherit; font-size: 13px; }
.kind select:focus-visible { outline: none; box-shadow: var(--ferrum-focus-ring, 0 0 0 3px rgb(47 91 234 / .35)); }
.inote { margin: 2px 6px 8px; padding: 8px 10px; border-radius: 8px; background: var(--ferrum-surface-2, #eef1f5); color: var(--ferrum-muted, #5b6472); font-size: 12.5px; }
.vh { position: absolute; width: 1px; height: 1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }
.side { position: fixed; top: var(--ferrum-bar-height, 48px); right: 0; bottom: 0; width: var(--ferrum-panel-width, 380px); max-width: 100vw;
  display: flex; flex-direction: column; background: var(--ferrum-surface, #fff); border-left: 1px solid var(--ferrum-border, #d9dee5);
  box-shadow: -8px 0 24px -16px rgb(16 24 40 / .35); }
.side header { display: flex; align-items: center; gap: 6px; height: 44px; padding: 0 6px 0 12px; border-bottom: 1px solid var(--ferrum-border, #d9dee5); }
.side header strong { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-weight: 650; }
.side iframe { flex: 1; width: 100%; border: 0; background: var(--ferrum-surface, #fff); }
.grip { position: absolute; left: -4px; top: 0; bottom: 0; width: 8px; cursor: ew-resize; touch-action: none; }
.grip:hover, .grip:focus-visible { background: linear-gradient(90deg, transparent 3px, var(--ferrum-primary, #2f5bea) 3px 5px, transparent 5px); outline: none; }
@media (max-width: 640px) { .search { flex: none; width: 34px; min-width: 34px; padding: 0; margin: 0; justify-content: center; background: transparent; border-color: transparent; }
  .search .search-label, .search kbd { display: none; } }
@media (prefers-reduced-motion: reduce) { .rgroup svg { animation: none; } }
@media (max-width: 640px) { .home, .sep, .crumbs { display: none; } .launcher { left: 4px; right: 4px; width: auto; } .apps { grid-template-columns: repeat(3, minmax(0, 1fr)); } }
`;

// Icons from Lucide (lucide.dev, ISC license, Copyright (c) Lucide Icons and Contributors): the
// elements inside each 24×24 icon, for the bar and for apps' launcher entries (`icon` in app.yaml).
const ICONS = {
  "layout-grid": '<rect width="7" height="7" x="3" y="3" rx="1"/><rect width="7" height="7" x="14" y="3" rx="1"/><rect width="7" height="7" x="14" y="14" rx="1"/><rect width="7" height="7" x="3" y="14" rx="1"/>',
  "plus": '<path d="M5 12h14"/><path d="M12 5v14"/>',
  "x": '<path d="M18 6 6 18"/><path d="m6 6 12 12"/>',
  "external-link": '<path d="M15 3h6v6"/><path d="M10 14 21 3"/><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>',
  "upload": '<path d="M12 3v12"/><path d="m17 8-5-5-5 5"/><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/>',
  "log-out": '<path d="m16 17 5-5-5-5"/><path d="M21 12H9"/><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/>',
  "user": '<path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/>',
  "settings": '<path d="M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"/><circle cx="12" cy="12" r="3"/>',
  "check": '<path d="M20 6 9 17l-5-5"/>',
  "circle-alert": '<circle cx="12" cy="12" r="10"/><line x1="12" x2="12" y1="8" y2="12"/><line x1="12" x2="12.01" y1="16" y2="16"/>',
  "loader-circle": '<path d="M21 12a9 9 0 1 1-6.219-8.56"/>',
  "shopping-bag": '<path d="M16 10a4 4 0 0 1-8 0"/><path d="M3.103 6.034h17.794"/><path d="M3.4 5.467a2 2 0 0 0-.4 1.2V20a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V6.667a2 2 0 0 0-.4-1.2l-2-2.667A2 2 0 0 0 17 2H7a2 2 0 0 0-1.6.8z"/>',
  "search": '<path d="m21 21-4.34-4.34"/><circle cx="11" cy="11" r="8"/>',
  "mail": '<path d="m22 7-8.991 5.727a2 2 0 0 1-2.009 0L2 7"/><rect x="2" y="4" width="20" height="16" rx="2"/>',
  "message-square": '<path d="M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z"/>',
  "calendar": '<path d="M8 2v3"/><path d="M16 2v3"/><rect x="3" y="3" width="18" height="18" rx="2"/><path d="M3 9h18"/>',
  "video": '<path d="m16 13 5.223 3.482a.5.5 0 0 0 .777-.416V7.87a.5.5 0 0 0-.752-.432L16 10.5"/><rect x="2" y="6" width="14" height="12" rx="2"/>',
  "file-text": '<path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z"/><path d="M14 2v5a1 1 0 0 0 1 1h5"/><path d="M10 9H8"/><path d="M16 13H8"/><path d="M16 17H8"/>',
  "hard-drive": '<path d="M10 16h.01"/><path d="M2.212 11.577a2 2 0 0 0-.212.896V18a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-5.527a2 2 0 0 0-.212-.896L18.55 5.11A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/><path d="M21.946 12.013H2.054"/><path d="M6 16h.01"/>',
  "sheet": '<rect width="18" height="18" x="3" y="3" rx="2" ry="2"/><line x1="3" x2="21" y1="9" y2="9"/><line x1="3" x2="21" y1="15" y2="15"/><line x1="9" x2="9" y1="9" y2="21"/><line x1="15" x2="15" y1="9" y2="21"/>',
  "presentation": '<path d="M2 3h20"/><path d="M21 3v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V3"/><path d="m7 21 5-5 5 5"/>',
  "building-2": '<path d="M10 12h4"/><path d="M10 8h4"/><path d="M14 21v-3a2 2 0 0 0-4 0v3"/><path d="M6 10H4a2 2 0 0 0-2 2v7a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2h-2"/><path d="M6 21V5a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v16"/>',
  "users": '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><path d="M16 3.128a4 4 0 0 1 0 7.744"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><circle cx="9" cy="7" r="4"/>',
  "headset": '<path d="M3 11h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-5Zm0 0a9 9 0 1 1 18 0m0 0v5a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3Z"/><path d="M21 16v2a4 4 0 0 1-4 4h-5"/>',
  "contact": '<path d="M16 2v2"/><path d="M7 21v-2a2 2 0 012-2h6a2 2 0 012 2v2"/><path d="M8 2v2"/><circle cx="12" cy="10" r="3"/><rect x="3" y="3" width="18" height="18" rx="2"/>',
  "folder-kanban": '<path d="M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.93a2 2 0 0 1-1.66-.9l-.82-1.2A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13c0 1.1.9 2 2 2Z"/><path d="M8 10v4"/><path d="M12 10v2"/><path d="M16 10v6"/>',
  "shopping-cart": '<path d="m2.05 2.05 1.099-.028a1 1 0 0 1 1.008.815l2.69 14.347A1 1 0 0 0 7.83 18H18"/><path d="M4.563 5h16.435a1 1 0 0 1 .981 1.204l-1.026 6.226A2 2 0 0 1 18.962 14H6.25"/><circle cx="18" cy="20" r="2"/><circle cx="8" cy="20" r="2"/>',
  "store": '<path d="M15 21v-5a1 1 0 0 0-1-1h-4a1 1 0 0 0-1 1v5"/><path d="M17.774 10.31a1.12 1.12 0 0 0-1.549 0 2.5 2.5 0 0 1-3.451 0 1.12 1.12 0 0 0-1.548 0 2.5 2.5 0 0 1-3.452 0 1.12 1.12 0 0 0-1.549 0 2.5 2.5 0 0 1-3.77-3.248l2.889-4.184A2 2 0 0 1 7 2h10a2 2 0 0 1 1.653.873l2.895 4.192a2.5 2.5 0 0 1-3.774 3.244"/><path d="M4 10.95V19a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-8.05"/>',
  "package": '<path d="M11 21.73a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73z"/><path d="M12 22V12"/><polyline points="3.29 7 12 12 20.71 7"/><path d="m7.5 4.27 9 5.15"/>',
  "factory": '<path d="M12 16h.01"/><path d="M16 16h.01"/><path d="M3 19a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V8.5a.5.5 0 0 0-.769-.422l-4.462 2.844A.5.5 0 0 1 15 10.5v-2a.5.5 0 0 0-.769-.422L9.77 10.922A.5.5 0 0 1 9 10.5V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2z"/><path d="M8 16h.01"/>',
  "landmark": '<path d="M10 18v-7"/><path d="M11.119 2.205a2 2 0 0 1 1.762 0l7.84 3.846A.5.5 0 0 1 20.5 7h-17a.5.5 0 0 1-.22-.949z"/><path d="M14 18v-7"/><path d="M18 18v-7"/><path d="M3 22h18"/><path d="M6 18v-7"/>',
  "receipt": '<path d="M12 17V7"/><path d="M16 8h-6a2 2 0 0 0 0 4h4a2 2 0 0 1 0 4H8"/><path d="M4 3a1 1 0 0 1 1-1 1.3 1.3 0 0 1 .7.2l.933.6a1.3 1.3 0 0 0 1.4 0l.934-.6a1.3 1.3 0 0 1 1.4 0l.933.6a1.3 1.3 0 0 0 1.4 0l.933-.6a1.3 1.3 0 0 1 1.4 0l.934.6a1.3 1.3 0 0 0 1.4 0l.933-.6A1.3 1.3 0 0 1 19 2a1 1 0 0 1 1 1v18a1 1 0 0 1-1 1 1.3 1.3 0 0 1-.7-.2l-.933-.6a1.3 1.3 0 0 0-1.4 0l-.934.6a1.3 1.3 0 0 1-1.4 0l-.933-.6a1.3 1.3 0 0 0-1.4 0l-.933.6a1.3 1.3 0 0 1-1.4 0l-.934-.6a1.3 1.3 0 0 0-1.4 0l-.933.6a1.3 1.3 0 0 1-.7.2 1 1 0 0 1-1-1z"/>',
  "calculator": '<rect width="16" height="20" x="4" y="2" rx="2"/><line x1="8" x2="16" y1="6" y2="6"/><line x1="16" x2="16" y1="14" y2="18"/><path d="M16 10h.01"/><path d="M12 10h.01"/><path d="M8 10h.01"/><path d="M12 14h.01"/><path d="M8 14h.01"/><path d="M12 18h.01"/><path d="M8 18h.01"/>',
  "clipboard-list": '<rect width="8" height="4" x="8" y="2" rx="1" ry="1"/><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/><path d="M12 11h4"/><path d="M12 16h4"/><path d="M8 11h.01"/><path d="M8 16h.01"/>',
  "signature": '<path d="m21 17-2.156-1.868A.5.5 0 0 0 18 15.5v.5a1 1 0 0 1-1 1h-2a1 1 0 0 1-1-1c0-2.545-3.991-3.97-8.5-4a1 1 0 0 0 0 5c4.153 0 4.745-11.295 5.708-13.5a2.5 2.5 0 1 1 3.31 3.284"/><path d="M3 21h18"/>',
  "graduation-cap": '<path d="M21.42 10.922a1 1 0 0 0-.019-1.838L12.83 5.18a2 2 0 0 0-1.66 0L2.6 9.08a1 1 0 0 0 0 1.832l8.57 3.908a2 2 0 0 0 1.66 0z"/><path d="M22 10v6"/><path d="M6 12.5V16a6 3 0 0 0 12 0v-3.5"/>',
  "wrench": '<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.106-3.105c.32-.322.863-.22.983.218a6 6 0 0 1-8.259 7.057l-7.91 7.91a1 1 0 0 1-2.999-3l7.91-7.91a6 6 0 0 1 7.057-8.259c.438.12.54.662.219.984z"/>',
  "boxes": '<path d="M2.97 12.92A2 2 0 0 0 2 14.63v3.24a2 2 0 0 0 .97 1.71l3 1.8a2 2 0 0 0 2.06 0L12 19v-5.5l-5-3-4.03 2.42Z"/><path d="m7 16.5-4.74-2.85"/><path d="m7 16.5 5-3"/><path d="M7 16.5v5.17"/><path d="M12 13.5V19l3.97 2.38a2 2 0 0 0 2.06 0l3-1.8a2 2 0 0 0 .97-1.71v-3.24a2 2 0 0 0-.97-1.71L17 10.5l-5 3Z"/><path d="m17 16.5-5-3"/><path d="m17 16.5 4.74-2.85"/><path d="M17 16.5v5.17"/><path d="M7.97 4.42A2 2 0 0 0 7 6.13v4.37l5 3 5-3V6.13a2 2 0 0 0-.97-1.71l-3-1.8a2 2 0 0 0-2.06 0l-3 1.8Z"/><path d="M12 8 7.26 5.15"/><path d="m12 8 4.74-2.85"/><path d="M12 13.5V8"/>',
  "calendar-check": '<path d="M8 2v3"/><path d="M16 2v3"/><rect x="3" y="3" width="18" height="18" rx="2"/><path d="M3 9h18"/><path d="m9 15 2 2 4-4"/>',
  "megaphone": '<path d="M11 6a13 13 0 0 0 8.4-2.8A1 1 0 0 1 21 4v12a1 1 0 0 1-1.6.8A13 13 0 0 0 11 14H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2z"/><path d="M6 14a12 12 0 0 0 2.4 7.2 2 2 0 0 0 3.2-2.4A8 8 0 0 1 10 14"/><path d="M8 6v8"/>',
  "layout-dashboard": '<rect width="7" height="9" x="3" y="3" rx="1"/><rect width="7" height="5" x="14" y="3" rx="1"/><rect width="7" height="9" x="14" y="12" rx="1"/><rect width="7" height="5" x="3" y="16" rx="1"/>',
  "sparkles": '<path d="M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z"/><path d="M20 2v4"/><path d="M22 4h-4"/><circle cx="4" cy="20" r="2"/>',
  "folder": '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  "chevron-right": '<path d="m9 18 6-6-6-6"/>',
  "chevron-left": '<path d="m15 18-6-6 6-6"/>',
  "list": '<path d="M3 12h.01"/><path d="M3 18h.01"/><path d="M3 6h.01"/><path d="M8 12h13"/><path d="M8 18h13"/><path d="M8 6h13"/>',
  "bell": '<path d="M10.268 21a2 2 0 0 0 3.464 0"/><path d="M3.262 15.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673C19.41 13.956 18 12.499 18 8A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326"/>',
  "check-check": '<path d="M18 6 7 17l-5-5"/><path d="m22 10-7.5 7.5L13 16"/>',
  "circle-check": '<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>',
};

const SVG_NS = "http://www.w3.org/2000/svg";

/** A Lucide icon by name, or null for a name this runtime does not have. */
function icon(name, size = 18) {
  const shape = ICONS[name];
  if (!shape) return null;
  const svg = document.createElementNS(SVG_NS, "svg");
  for (const [k, v] of Object.entries({ width: size, height: size, viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": 2, "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" }))
    svg.setAttribute(k, String(v));
  svg.innerHTML = shape;
  return svg;
}

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
  if (/^#[0-9a-f]{6}$/i.test(entry?.color ?? "")) return entry.color;
  let h = 0;
  for (const c of entry?.title ?? entry?.app ?? "") h = (h * 31 + c.charCodeAt(0)) % 360;
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

/** An app's tile: its icon (or its initial) on its colour. */
function tile(entry, small = false) {
  return el(
    "span",
    { class: small ? "tile small" : "tile", "aria-hidden": "true", style: `background:${colourOf(entry)}` },
    icon(entry?.icon, small ? 15 : 22) ?? document.createTextNode((entry?.title ?? "?").slice(0, 1).toUpperCase()),
  );
}

/** Whether this page is at or under an address (a launcher entry's `/mail/`). */
function under(url) {
  const base = String(url ?? "").replace(/\/+$/, "");
  return base !== "" && (location.pathname === base || location.pathname.startsWith(`${base}/`));
}

/** The launcher entry of the page's app: the one the page is under (Mail, not Chat), else its first. */
function ownEntry() {
  const apps = (shellData?.apps ?? []).filter((a) => a.app === boot?.app?.id);
  return apps.filter((a) => under(a.url)).sort((a, b) => b.url.length - a.url.length)[0] ?? apps[0];
}

// ───────────────────────── the bar's search (§12.8) ─────────────────────────

// Apps from the launcher, the records the person may read (/api/search), and what the apps'
// providers find (`shell.search`, asked here in parallel), each group drawn as it arrives.
const finder = { q: "", ctl: null, timer: 0, groups: [], active: 0, input: null, list: null };

/** A same-site page to open, or null (a provider's answer is not trusted further). */
const sitePath = (url) => (typeof url === "string" && url.startsWith("/") && !url.startsWith("//") ? url : null);

function openFinder(b) {
  b.searchPanel.hidden = false;
  b.searchBtn.setAttribute("aria-expanded", "true");
  const input = el("input", {
    type: "search",
    placeholder: "Search apps, records and more",
    "aria-label": "Search",
    role: "combobox",
    "aria-expanded": "true",
    "aria-controls": "ferrum-results",
    "aria-autocomplete": "list",
    autocomplete: "off",
    spellcheck: "false",
  });
  const list = el("div", { class: "results", id: "ferrum-results", role: "listbox", "aria-label": "Results" });
  b.searchPanel.replaceChildren(el("div", { class: "find" }, icon("search", 16), input), list);
  Object.assign(finder, { input, list });
  input.value = finder.q;
  input.addEventListener("input", () => {
    finder.q = input.value;
    clearTimeout(finder.timer);
    finder.timer = setTimeout(() => runFinder(), 180);
    finder.groups = finder.groups.filter((g) => g.key !== "apps");
    finder.groups.unshift({ key: "apps", title: "Apps", items: appsMatching(input.value) });
    finder.active = 0;
    drawFinder();
  });
  input.addEventListener("keydown", (e) => {
    const items = finder.groups.flatMap((g) => g.items);
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!items.length) return;
      finder.active = (finder.active + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length;
      drawFinder();
    } else if (e.key === "Enter") {
      const item = items[finder.active];
      if (item) {
        e.preventDefault();
        openFound(item);
      }
    }
  });
  runFinder();
  input.focus();
  input.select();
  if (!shellData) loadShell().then(() => runFinder());
}

function appsMatching(q) {
  const t = q.trim().toLowerCase();
  if (!t) return [];
  return (shellData?.apps ?? [])
    .filter((a) => `${a.title} ${a.description ?? ""}`.toLowerCase().includes(t))
    .slice(0, 6)
    .map((a) => ({ title: a.title, subtitle: a.description ?? "App", url: a.url, app: a }));
}

function runFinder() {
  finder.ctl?.abort();
  const ctl = new AbortController();
  finder.ctl = ctl;
  const q = finder.q.trim();
  finder.active = 0;
  finder.groups = [{ key: "apps", title: "Apps", items: appsMatching(q) }];
  if (q.length >= 2) {
    const sources = [
      { key: "records", title: "Records", url: `/api/search?q=${encodeURIComponent(q)}&limit=12` },
      ...(shellData?.search ?? [])
        .filter((p) => typeof p.endpoint === "string" && p.endpoint.startsWith("/api/"))
        .map((p) => ({ key: `${p.app}.${p.id}`, title: p.title ?? appTitle_(p.app), url: `${p.endpoint}?q=${encodeURIComponent(q)}&limit=8` })),
    ];
    for (const s of sources) {
      const group = { key: s.key, title: s.title, items: [], loading: true };
      finder.groups.push(group);
      // A provider that takes too long is left out of this search, not waited for.
      const slow = setTimeout(() => group.loading && ((group.loading = false), finder.ctl === ctl && drawFinder()), 2500);
      const items = (list) =>
        (Array.isArray(list) ? list : [])
          .map((r) => ({ title: String(r.title ?? ""), subtitle: r.subtitle ? String(r.subtitle) : "", url: sitePath(r.url), icon: r.icon }))
          .filter((r) => r.title && r.url)
          .slice(0, 12);
      fetch(s.url, { credentials: "same-origin", headers: { accept: "application/json" }, signal: ctl.signal })
        .then((r) => (r.ok ? r.json() : null))
        .then((body) => {
          group.items = items(body?.results);
          // The lists the person may open, by name: "Go to", before the records.
          const pages = s.key === "records" ? items(body?.pages).map((p) => ({ ...p, icon: p.icon && ICONS[p.icon] ? p.icon : "list" })) : [];
          if (pages.length && finder.ctl === ctl) finder.groups.splice(finder.groups.indexOf(group), 0, { key: "pages", title: "Go to", items: pages, loading: false });
        })
        .catch(() => {})
        .finally(() => {
          clearTimeout(slow);
          group.loading = false;
          if (finder.ctl === ctl) drawFinder();
        });
    }
  }
  drawFinder();
}

/** `text` with each occurrence of `q` marked (text nodes only). */
function marked(text, q) {
  const out = el("strong");
  const t = q.trim();
  if (!t) return (out.textContent = text), out;
  const low = text.toLowerCase();
  let at = 0;
  for (let i = low.indexOf(t.toLowerCase()); i >= 0; i = low.indexOf(t.toLowerCase(), at)) {
    out.append(text.slice(at, i), el("mark", { text: text.slice(i, i + t.length) }));
    at = i + t.length;
  }
  out.append(text.slice(at));
  return out;
}

function drawFinder() {
  if (!finder.list) return;
  const q = finder.q.trim();
  const rows = [];
  let n = 0;
  for (const g of finder.groups) {
    if (!g.items.length && !g.loading) continue;
    rows.push(el("p", { class: "rgroup", role: "presentation" }, el("span", { text: g.title }), g.loading ? icon("loader-circle", 12) : null));
    for (const item of g.items) {
      const index = n++;
      const glyph = item.app ? tile(item.app, true) : el("span", { class: "glyph", "aria-hidden": "true" }, icon(item.icon && ICONS[item.icon] ? item.icon : "file-text", 15));
      rows.push(
        el(
          "a",
          {
            class: "result",
            href: item.url,
            role: "option",
            id: `ferrum-result-${index}`,
            "aria-selected": index === finder.active ? "true" : "false",
            onclick: (e) => (e.preventDefault(), openFound(item)),
            onpointermove: () => {
              if (finder.active !== index) {
                finder.active = index;
                drawFinder();
              }
            },
          },
          glyph,
          el("span", { class: "text" }, marked(item.title, q), item.subtitle ? el("small", { text: item.subtitle }) : null),
        ),
      );
    }
  }
  if (!rows.length) {
    const busy = finder.groups.some((g) => g.loading);
    rows.push(el("p", { class: "rhint", text: q.length < 2 ? "Type to search apps, records, conversations and files." : busy ? "Searching…" : `Nothing found for “${q}”.` }));
  }
  finder.list.replaceChildren(...rows);
  finder.input?.setAttribute("aria-activedescendant", n ? `ferrum-result-${finder.active}` : "");
  finder.list.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: "nearest" });
}

function openFound(item) {
  bar()?.closePanels(false);
  navigate(item.url);
}

// The bar keeps the search shortcut (§12.2): before a page's own handlers see it.
window.addEventListener(
  "keydown",
  (e) => {
    if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey || e.key.toLowerCase() !== "k") return;
    const b = bar();
    if (!b?.searchBtn || b.hidden) return;
    e.preventDefault();
    e.stopPropagation();
    b.toggleSearch();
  },
  true,
);

// ───────────────────────── notifications (§12.8) ─────────────────────────

// The bell: how many are unread (on idle, then from the live channel), the latest when it opens,
// each opening its record through /open, and how each kind reaches the person. Tabs tell each
// other what the person read; each hears new ones on the live channel itself.
const inbox = { unread: 0, items: null, failed: false, types: null, view: "list", status: "" };
const inboxTabs = "BroadcastChannel" in window ? new BroadcastChannel("ferrum-inbox") : null;
const CHANNEL_LABELS = { inapp: "In Ferrum", email: "Email at once", digest: "Daily email digest", off: "Off" };

inboxTabs?.addEventListener("message", (e) => {
  setUnread(e.data?.unread, false);
  inbox.items = null;
  if (bar()?.inboxPanel?.hidden === false && inbox.view === "list") loadInbox();
});

function setUnread(n, share) {
  inbox.unread = Math.max(0, Number(n) || 0);
  bar()?.drawBell();
  if (share) inboxTabs?.postMessage({ unread: inbox.unread });
}

const answer = (r) => (r?.ok ? r.json() : (r?.status === 401 && toLogin(), null));

async function countInbox() {
  const body = await fetch("/api/notifications?limit=0", { credentials: "same-origin" }).then(answer).catch(() => null);
  if (body) setUnread(body.unread, false);
}

async function loadInbox() {
  const body = await fetch("/api/notifications?limit=50", { credentials: "same-origin" }).then(answer).catch(() => null);
  inbox.failed = !body;
  if (body) {
    inbox.items = Array.isArray(body.notifications) ? body.notifications : [];
    setUnread(body.unread, false);
  }
  drawInbox();
}

/** "5 min ago", "yesterday", "12 Oct". */
function ago(iso) {
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return "";
  const s = (t - Date.now()) / 1000;
  const a = Math.abs(s);
  const lang = boot?.locale?.language ?? "en";
  const rtf = new Intl.RelativeTimeFormat(lang, { numeric: "auto", style: "short" });
  if (a < 60) return "just now";
  if (a < 3600) return rtf.format(Math.round(s / 60), "minute");
  if (a < 86400) return rtf.format(Math.round(s / 3600), "hour");
  if (a < 7 * 86400) return rtf.format(Math.round(s / 86400), "day");
  return new Date(t).toLocaleDateString(lang, { day: "numeric", month: "short" });
}

const noteUrl = (n) => (n.entity && n.record_id ? `/open/${encodeURIComponent(n.entity)}/${encodeURIComponent(n.record_id)}` : null);

function openNote(n) {
  const url = noteUrl(n);
  if (!n.read_at) {
    n.read_at = new Date().toISOString();
    setUnread(inbox.unread - 1, true);
    // keepalive: the request outlives the page it is leaving.
    fetch(`/api/notifications/${encodeURIComponent(n.id)}/read`, { method: "POST", credentials: "same-origin", keepalive: true }).catch(() => {});
  }
  if (url) {
    bar()?.closePanels(false);
    navigate(url);
  } else drawInbox();
}

async function readAll() {
  const r = await fetch("/api/notifications/read-all", { method: "POST", credentials: "same-origin" }).catch(() => null);
  if (!r?.ok) {
    inbox.status = "Could not mark them read. Try again.";
    return drawInbox();
  }
  for (const n of inbox.items ?? []) n.read_at ??= new Date().toISOString();
  inbox.status = "";
  setUnread(0, true);
  drawInbox();
}

async function loadKinds() {
  inbox.types = null;
  drawInbox();
  inbox.types = (await fetch("/api/notifications/types", { credentials: "same-origin" }).then(answer).catch(() => null)) ?? { failed: true };
  drawInbox();
}

async function chooseChannel(kind, select) {
  const before = kind.channel;
  kind.channel = select.value;
  select.disabled = true;
  const body = JSON.stringify({ kind: kind.kind, channel: kind.channel === kind.default ? null : kind.channel });
  const r = await fetch("/api/notifications/preferences", { method: "PUT", credentials: "same-origin", headers: { "content-type": "application/json" }, body }).catch(() => null);
  select.disabled = false;
  inbox.status = r?.ok ? "" : "Could not save that. Try again.";
  if (!r?.ok) {
    kind.channel = before;
    select.value = before;
  }
  bar()?.inboxPanel?.querySelector(".istatus")?.replaceChildren(inbox.status);
}

function drawInbox() {
  const p = bar()?.inboxPanel;
  if (!p || p.hidden) return;
  const focused = p.contains(p.getRootNode().activeElement);
  const status = el("p", { class: "istatus", role: "status", text: inbox.status });
  if (inbox.view === "settings") {
    const t = inbox.types;
    const kinds = el("div", { class: "kinds" });
    if (!t) kinds.append(el("p", { class: "rhint", text: "Loading…" }));
    else if (t.failed) kinds.append(el("p", { class: "rhint", text: "Could not load your choices." }));
    else {
      if (!t.email) kinds.append(el("p", { class: "inote", text: "This server sends no email, so notifications stay in Ferrum." }));
      let group;
      for (const k of t.types ?? []) {
        if ((k.app ?? "") !== group) kinds.append(el("p", { class: "rgroup", text: k.app ?? "All apps" }));
        group = k.app ?? "";
        const id = `ferrum-kind-${String(k.kind).replace(/[^a-z0-9_-]/gi, "-")}`;
        const select = el("select", { id, "aria-describedby": `${id}-hint` });
        for (const c of t.channels ?? []) {
          const mail = c === "email" || c === "digest";
          select.append(el("option", { value: c, disabled: mail && !t.email, text: CHANNEL_LABELS[c] ?? c }));
        }
        select.value = k.channel;
        const hint = el("small", { id: `${id}-hint` });
        const showDefault = () => (hint.textContent = k.channel === k.default ? "" : `Default: ${CHANNEL_LABELS[k.default] ?? k.default}`);
        showDefault();
        select.addEventListener("change", () => chooseChannel(k, select).then(showDefault));
        kinds.append(el("div", { class: "kind" }, el("span", { class: "what" }, el("label", { for: id, text: k.label }), hint), select));
      }
    }
    const back = el("button", { type: "button", class: "ghost", "aria-label": "Back to notifications", title: "Back", onclick: () => ((inbox.view = "list"), (inbox.status = ""), loadInbox()) }, icon("chevron-left", 16));
    p.replaceChildren(el("div", { class: "ihead" }, back, el("h2", { text: "How notifications reach you" })), status, kinds);
    if (focused) back.focus();
    return;
  }
  const allRead = el("button", { type: "button", class: "ghost", title: "Mark all as read", "aria-label": "Mark all as read", disabled: !inbox.unread, onclick: () => readAll() }, icon("check-check", 16));
  const settings = el("button", { type: "button", class: "ghost", title: "Notification settings", "aria-label": "Notification settings", onclick: () => ((inbox.view = "settings"), (inbox.status = ""), loadKinds()) }, icon("settings", 16));
  const list = el("ul", { class: "ilist", "aria-label": "Notifications" });
  if (!inbox.items)
    list.append(
      el("li", { class: "rhint" }, inbox.failed ? el("span", {}, "Could not load your notifications. ", el("button", { type: "button", class: "link", text: "Try again", onclick: () => loadInbox() })) : "Loading…"),
    );
  else if (!inbox.items.length)
    list.append(el("li", { class: "caught" }, icon("circle-check", 28), el("strong", { text: "You're all caught up" }), el("small", { text: "Mentions, assignments and changes to what you follow show up here." })));
  for (const n of inbox.items ?? []) {
    const url = noteUrl(n);
    list.append(
      el(
        "li",
        {},
        el(
          url ? "a" : "button",
          { class: n.read_at ? "note" : "note unread", href: url, type: url ? null : "button", onclick: (e) => (e.preventDefault(), openNote(n)) },
          el("span", { class: "dot", "aria-hidden": "true" }),
          el(
            "span",
            { class: "text" },
            el("strong", {}, n.read_at ? null : el("span", { class: "vh", text: "Unread: " }), String(n.title ?? "")),
            n.body ? el("small", { class: "body", text: String(n.body) }) : null,
            el("small", { class: "when", text: ago(n.created_at) }),
          ),
        ),
      ),
    );
  }
  p.replaceChildren(el("div", { class: "ihead" }, el("h2", { text: "Notifications" }), allRead, settings), status, list);
  if (focused) (p.querySelector(".note") ?? settings).focus();
}

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
    this.tileEl = el("span");
    this.launchBtn = el("button", { class: "launch", type: "button", "aria-label": "Apps", title: "Apps", "aria-haspopup": "dialog", "aria-expanded": "false", onclick: () => this.toggleLauncher() }, icon("layout-grid", 19));
    this.meBtn = el("button", {
      class: "me",
      type: "button",
      "aria-label": `Account: ${person.name ?? ""}`,
      title: person.name ?? "",
      "aria-haspopup": "menu",
      "aria-expanded": "false",
      text: initials(person.name),
      onclick: () => this.toggleMenu(),
    });
    const mac = /Mac|iPhone|iPad/.test(navigator.platform ?? "");
    this.searchBtn = el(
      "button",
      { class: "search", type: "button", "aria-label": "Search", title: `Search (${mac ? "⌘K" : "Ctrl+K"})`, "aria-haspopup": "dialog", "aria-expanded": "false", onclick: () => this.toggleSearch() },
      icon("search", 16),
      el("span", { class: "search-label", text: "Search" }),
      el("kbd", { text: mac ? "⌘K" : "Ctrl K" }),
    );
    // The bell, for people signed in (not on a public page).
    this.bellBtn =
      person.id && hostKind !== "public"
        ? el("button", { class: "bell", type: "button", "aria-label": "Notifications", title: "Notifications", "aria-haspopup": "dialog", "aria-expanded": "false", onclick: () => this.toggleInbox() }, icon("bell", 18))
        : null;
    const appRoute = boot?.app?.routes?.[boot?.app?.entry] ?? "/";
    const bar = el(
      "div",
      { class: "bar", role: "banner" },
      el("a", { class: "skip", href: "#main", text: "Skip to content", onclick: (e) => this.skip(e) }),
      this.launchBtn,
      el("a", { class: "home", href: "/", title: "Home", text: boot?.workspace?.name ?? "Home" }),
      // A page of no app (a status page) shows the workspace alone.
      boot?.app ? el("span", { class: "sep", "aria-hidden": "true", text: "/" }) : null,
      (this.appLink = el("a", { class: "app", href: appRoute, hidden: !boot?.app }, this.tileEl, this.titleEl)),
      this.crumbsEl,
      el("span", { class: "spacer" }),
      this.searchBtn,
      el("span", { class: "spacer" }),
      this.indicators,
      (this.tools = el("span", { class: "tools" })),
      this.bellBtn,
      this.meBtn,
    );
    this.launcher = el("div", { class: "panel launcher", role: "dialog", "aria-label": "Apps", hidden: true });
    this.menu = el("div", { class: "panel menu", role: "menu", "aria-label": "Account", hidden: true });
    this.createMenu = el("div", { class: "panel menu create", role: "menu", "aria-label": "Create", hidden: true });
    this.tray = el("div", { class: "panel tray", role: "dialog", "aria-label": "Uploads", hidden: true });
    this.searchPanel = el("div", { class: "panel searchbox", role: "dialog", "aria-label": "Search", hidden: true });
    this.inboxPanel = el("div", { class: "panel inbox", role: "dialog", "aria-label": "Notifications", hidden: true });
    this.side = el("aside", { class: "side", "aria-label": "Side panel", hidden: true });
    this.noticeEl = el("div", { class: "notice", role: "status", hidden: true });
    root.append(style, bar, this.launcher, this.menu, this.createMenu, this.tray, this.searchPanel, this.inboxPanel, this.side, this.noticeEl);
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
    if (this.bellBtn) {
      (window.requestIdleCallback ?? ((f) => setTimeout(f, 1200)))(() => countInbox());
      // A new one: counted, listed if the bell is open; one that asks for the person, said.
      subscribe(
        ["notification"],
        (d) => {
          setUnread(inbox.unread + 1, false);
          inbox.items = null;
          if (!this.inboxPanel.hidden && inbox.view === "list") loadInbox();
          const n = d.notification ?? {};
          if (["mention", "assignment"].includes(n.kind) && this.inboxPanel.hidden) {
            const url = noteUrl(n);
            this.notice(String(n.title ?? "New notification"), url ? [["Open", () => (this.hideNotice(), openNote(n))]] : []);
            this.noticeFrom = "inbox";
            clearTimeout(this.noticeTimer);
            this.noticeTimer = setTimeout(() => this.hideNotice(), 8000);
          }
        },
        { resync: () => countInbox() },
      );
    }
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
    const wasOpen = !this.launcher.hidden ? this.launchBtn : !this.menu.hidden ? this.meBtn : !this.createMenu.hidden ? this.createBtn : !this.inboxPanel.hidden ? this.bellBtn : null;
    this.launcher.hidden = true;
    this.menu.hidden = true;
    this.createMenu.hidden = true;
    this.tray.hidden = true;
    this.inboxPanel.hidden = true;
    this.bellBtn?.setAttribute("aria-expanded", "false");
    if (!this.searchPanel.hidden) {
      this.searchPanel.hidden = true;
      finder.ctl?.abort();
      if (restoreFocus) this.searchBtn.focus();
    }
    this.searchBtn.setAttribute("aria-expanded", "false");
    this.launchBtn.setAttribute("aria-expanded", "false");
    this.meBtn.setAttribute("aria-expanded", "false");
    this.createBtn?.setAttribute("aria-expanded", "false");
    if (restoreFocus && wasOpen) wasOpen.focus();
  }

  /** The page's app in the bar: its tile, linking to where it starts (Mail's, once Comms shows Mail). */
  drawApp() {
    const own = boot?.app ? ownEntry() : null;
    this.tileEl?.replaceChildren(...(own ? [tile(own, true)] : []));
    if (own && this.appLink) this.appLink.href = own.url;
  }

  /** The page's app, the Create menu and the side panels' buttons, from the launcher's data. */
  drawTools() {
    if (!this.tools) return;
    this.drawApp();
    const create = shellData?.create ?? [];
    // Panels that need something to show (a file's comments) open from apps, not from the bar.
    const panels = (shellData?.sidepanels ?? []).filter((p) => !p.contextual);
    const apps = shellData?.apps ?? [];
    const panelEntry = (p) => apps.find((a) => a.app === p.app && a.id === p.id) ?? apps.find((a) => a.app === p.app);
    this.createBtn = create.length
      ? el("button", { type: "button", "aria-label": "Create", "aria-haspopup": "menu", "aria-expanded": "false", title: "Create", onclick: () => this.toggleCreate() }, icon("plus", 19))
      : null;
    const open = sidepanelOpen();
    this.tools.replaceChildren(
      ...[this.createBtn].filter(Boolean),
      ...panels.map((p) =>
        el(
          "button",
          {
            type: "button",
            "aria-pressed": open === p.id ? "true" : "false",
            "aria-label": p.title,
            title: p.title,
            "data-panel": p.id,
            onclick: () => (sidepanelOpen() === p.id ? closeSidepanel() : openSidepanel(p.id)),
          },
          icon(p.icon, 19) ?? icon(panelEntry(p)?.icon, 19) ?? document.createTextNode((p.title ?? "?").slice(0, 1).toUpperCase()),
        ),
      ),
    );
  }

  toggleInbox() {
    const open = this.inboxPanel.hidden;
    this.closePanels(false);
    if (!open || !this.bellBtn) return;
    if (this.noticeFrom === "inbox") this.hideNotice();
    this.inboxPanel.hidden = false;
    this.bellBtn.setAttribute("aria-expanded", "true");
    inbox.view = "list";
    inbox.status = "";
    drawInbox();
    this.inboxPanel.querySelector("button")?.focus();
    loadInbox();
  }

  /** The bell's count: a dot with the number (99+), and said with the button's name. */
  drawBell() {
    if (!this.bellBtn) return;
    const n = inbox.unread;
    this.bellBtn.querySelector(".badge")?.remove();
    if (n) this.bellBtn.append(el("span", { class: "badge", "aria-hidden": "true", text: n > 99 ? "99+" : String(n) }));
    const label = n ? `Notifications, ${n} unread` : "Notifications";
    this.bellBtn.setAttribute("aria-label", label);
    this.bellBtn.title = label;
  }

  toggleSearch() {
    const open = this.searchPanel.hidden;
    this.closePanels(false);
    if (open) openFinder(this);
  }

  toggleCreate() {
    const open = this.createMenu.hidden;
    this.closePanels(false);
    if (!open || !this.createBtn) return;
    const entryOf = (app) => (shellData?.apps ?? []).find((a) => a.app === app);
    this.createMenu.replaceChildren(
      ...(shellData?.create ?? []).map((c) =>
        el("a", { role: "menuitem", href: c.url, onclick: (e) => (e.preventDefault(), navigate(c.url)) }, tile(entryOf(c.app) ?? { title: c.label }, true), el("span", { text: c.label })),
      ),
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
      // Apps on their own first, then each family under its name (§11.5), its head first.
      const alone = found.filter((a) => !a.family);
      const families = [];
      for (const a of found.filter((a) => a.family)) if (!families.includes(a.family)) families.push(a.family);
      const items = [...alone];
      for (const f of families) {
        const fam = (shellData?.families ?? []).find((x) => x.id === f);
        const members = found.filter((b) => b.family === f).sort((x, y) => Number(x.app !== f) - Number(y.app !== f));
        if (fam && (alone.length || families.length > 1)) items.push({ group: fam.title });
        items.push(...members);
      }
      grid.replaceChildren(
        ...items.map((a) => {
          if (a.group) return el("p", { class: "group", role: "presentation", text: a.group });
          const here = under(a.url);
          const link = el(
            "a",
            { class: "tile-link", href: a.url, role: "listitem", "aria-current": here ? "page" : null, title: a.description ?? null },
            tile(a),
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
    const foot = el(
      "div",
      { class: "foot" },
      el("a", { href: "/" }, icon("layout-dashboard", 15), el("span", { text: "Home" })),
      boot?.person?.admin ? el("a", { href: "/store/" }, icon("shopping-bag", 15), el("span", { text: "Get apps" })) : null,
    );
    this.launcher.replaceChildren(el("div", { class: "find" }, icon("search", 16), search), grid, foot);
  }

  toggleMenu() {
    const open = this.menu.hidden;
    this.closePanels(false);
    if (!open) return;
    const person = boot?.person ?? {};
    this.menu.replaceChildren(
      el(
        "div",
        { class: "who" },
        el("span", { class: "me", "aria-hidden": "true", text: initials(person.name) }),
        el("div", {}, el("strong", { text: person.name ?? "" }), el("small", { text: person.email ?? "" }), el("small", { text: boot?.workspace?.name ?? "" })),
      ),
      el("a", { role: "menuitem", href: "/account/" }, icon("user", 16), el("span", { text: "Account and devices" })),
      el("hr"),
      el("button", { role: "menuitem", type: "button", onclick: () => guardedLeave(signOut) }, icon("log-out", 16), el("span", { text: "Sign out" })),
    );
    this.menu.hidden = false;
    this.meBtn.setAttribute("aria-expanded", "true");
    this.menu.querySelector("a, button")?.focus();
  }

  notice(text, actions = []) {
    if (!this.noticeEl) return;
    this.noticeFrom = null;
    this.noticeEl.replaceChildren(el("span", { text }), ...actions.map(([label, run]) => el("button", { type: "button", text: label, onclick: run })));
    this.noticeEl.hidden = false;
  }

  hideNotice() {
    if (this.noticeEl) this.noticeEl.hidden = true;
  }

  setIndicator(key, text, run, iconName) {
    if (!this.indicators) return;
    this.indicators.querySelector(`[data-key="${key}"]`)?.remove();
    if (text) this.indicators.append(el("button", { class: "indicator", "data-key": key, type: "button", onclick: run }, icon(iconName, 14), el("span", { text })));
  }
}

const bar = () => document.querySelector("ferrum-bar");

// ───────────────────────── record cards (§11.6) ─────────────────────────

// Hovering (or focusing) a link to a record — `/open/{Entity}/{id}`, or a records frame's
// `…/e/{Entity}/{id}` — shows its card after a moment: the app's own (`shell.card`) when it serves
// one, else the one the platform derives from metadata. It stays while the pointer is on the link
// or the card, and goes with Escape, scrolling or leaving. Not on touch: a tap opens the record.
const CARD_CSS = `
:host { all: initial; position: fixed; z-index: 2147483001; inset: 0 auto auto 0; pointer-events: none; }
.card { pointer-events: auto; box-sizing: border-box; width: min(340px, calc(100vw - 16px)); padding: 12px 14px; border-radius: 12px;
  background: var(--ferrum-surface, #fff); color: var(--ferrum-text, #17191d); border: 1px solid var(--ferrum-border, #d9dee5);
  box-shadow: var(--ferrum-shadow, 0 8px 28px rgb(16 24 40 / .14)); font: 13px/1.4 var(--ferrum-font, system-ui, sans-serif);
  -webkit-font-smoothing: antialiased; animation: in .12s ease-out; }
@keyframes in { from { opacity: 0; transform: translateY(3px); } to { opacity: 1; transform: none; } }
@media (prefers-reduced-motion: reduce) { .card { animation: none; } }
.head { display: flex; gap: 10px; align-items: flex-start; }
.glyph { flex: none; display: grid; place-items: center; width: 32px; height: 32px; border-radius: 9px; background: var(--ferrum-primary-soft, #e7edfd);
  color: var(--ferrum-primary, #2f5bea); }
.who { display: grid; gap: 2px; min-width: 0; }
.title { font-weight: 650; font-size: 14px; overflow-wrap: anywhere; }
.meta { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; color: var(--ferrum-muted, #5b6472); font-size: 12px; }
.state { padding: 1px 7px; border-radius: 999px; background: var(--ferrum-surface-2, #eef1f5); color: var(--ferrum-text, #17191d); font-weight: 500; }
dl { display: grid; grid-template-columns: minmax(0, 40%) minmax(0, 1fr); gap: 5px 12px; margin: 10px 0 0; padding-top: 10px;
  border-top: 1px solid var(--ferrum-border, #d9dee5); }
dt { color: var(--ferrum-muted, #5b6472); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
dd { margin: 0; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.foot { margin-top: 10px; color: var(--ferrum-muted, #5b6472); font-size: 11.5px; }
.busy { color: var(--ferrum-muted, #5b6472); }
`;
const RECORD_LINK = [/^\/open\/([A-Z][A-Za-z0-9]{0,79})\/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\/?$/, /\/e\/([A-Z][A-Za-z0-9]{0,79})\/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\/?$/];
const cardCache = new Map();
const hover = { link: null, timer: 0, hide: 0, ctl: null, host: null, root: null, touch: false };

/** The record a link opens, or null. */
function recordOf(a) {
  let url;
  try {
    url = new URL(a.href, location.href);
  } catch {
    return null;
  }
  if (url.origin !== location.origin) return null;
  for (const re of RECORD_LINK) {
    const m = url.pathname.match(re);
    if (m) return { entity: m[1], id: m[2] };
  }
  return null;
}

async function cardFor(entity, id, signal) {
  const key = `${entity}/${id}`;
  const hit = cardCache.get(key);
  if (hit && Date.now() - hit.at < 60_000) return hit.card;
  const own = (shellData?.cards ?? []).find((c) => c.entity === entity && typeof c.endpoint === "string" && c.endpoint.startsWith("/api/"));
  const url = own ? `${own.endpoint}?id=${encodeURIComponent(id)}` : `/api/cards/${encodeURIComponent(entity)}/${encodeURIComponent(id)}`;
  const r = await fetch(url, { credentials: "same-origin", headers: { accept: "application/json" }, signal });
  const card = r.ok ? await r.json() : null;
  if (card && typeof card.title === "string") cardCache.set(key, { at: Date.now(), card });
  if (cardCache.size > 200) cardCache.delete(cardCache.keys().next().value);
  return card && typeof card.title === "string" ? card : null;
}

/** A field's value as people read it. */
function shown(f) {
  const v = f?.value;
  const lang = boot?.locale?.language ?? undefined;
  if (v === true || v === false) return v ? "Yes" : "No";
  if (typeof v === "number" || (typeof v === "string" && /^-?\d+(\.\d+)?$/.test(v) && ["decimal", "currency", "int", "percent"].includes(f.type))) {
    const n = Number(v);
    const text = new Intl.NumberFormat(lang, { maximumFractionDigits: f.type === "int" ? 0 : 2, minimumFractionDigits: f.type === "currency" ? 2 : 0 }).format(n);
    return f.type === "percent" ? `${text}%` : text;
  }
  if (typeof v === "string" && f.type === "date" && /^\d{4}-\d{2}-\d{2}$/.test(v)) return new Date(`${v}T00:00:00`).toLocaleDateString(lang, { day: "numeric", month: "short", year: "numeric" });
  if (typeof v === "string" && f.type === "datetime" && Number.isFinite(Date.parse(v))) return new Date(v).toLocaleString(lang, { dateStyle: "medium", timeStyle: "short" });
  return typeof v === "string" ? v : v == null ? "" : JSON.stringify(v);
}

function cardRoot() {
  if (hover.root?.host.isConnected) return hover.root;
  const host = document.createElement("div");
  host.setAttribute("data-ferrum-card", "");
  host.setAttribute("aria-hidden", "true");
  const root = host.attachShadow({ mode: "open" });
  const style = document.createElement("style");
  style.textContent = CARD_CSS;
  root.append(style);
  host.addEventListener("pointerenter", () => clearTimeout(hover.hide));
  host.addEventListener("pointerleave", () => hideCardSoon());
  document.body.append(host);
  hover.host = host;
  hover.root = root;
  return root;
}

function placeCard(link, el_) {
  const r = link.getBoundingClientRect();
  const w = Math.min(340, innerWidth - 16);
  const left = Math.max(8, Math.min(r.left, innerWidth - w - 8));
  const below = innerHeight - r.bottom > 220 || r.top < 220;
  hover.host.style.transform = `translate(${Math.round(left)}px, ${Math.round(below ? r.bottom + 6 : r.top - 6)}px)`;
  el_.style.transform = below ? "" : "translateY(-100%)";
}

function drawCard(link, card) {
  const root = cardRoot();
  root.querySelector(".card")?.remove();
  const box = el("div", { class: "card" });
  if (!card) box.append(el("span", { class: "busy", text: "Loading…" }));
  else {
    const meta = el("div", { class: "meta" }, el("span", { text: [card.kind, card.number].filter((x) => typeof x === "string" && x).join(" · ") }), typeof card.state === "string" && card.state ? el("span", { class: "state", text: card.state }) : null);
    box.append(el("div", { class: "head" }, el("span", { class: "glyph" }, icon(card.icon, 17) ?? icon("file-text", 17)), el("div", { class: "who" }, el("span", { class: "title", text: card.title }), meta)));
    const fields = (Array.isArray(card.fields) ? card.fields : []).filter((f) => f && typeof f.label === "string" && shown(f) !== "").slice(0, 8);
    if (fields.length) box.append(el("dl", {}, ...fields.flatMap((f) => [el("dt", { text: f.label, title: f.label }), el("dd", { text: shown(f), title: shown(f) })])));
    if (typeof card.updated_at === "string" && ago(card.updated_at)) box.append(el("div", { class: "foot", text: `Changed ${ago(card.updated_at)}` }));
  }
  root.append(box);
  placeCard(link, box);
}

function hideCard() {
  clearTimeout(hover.timer);
  clearTimeout(hover.hide);
  hover.ctl?.abort();
  hover.link = null;
  hover.root?.querySelector(".card")?.remove();
}

function hideCardSoon() {
  clearTimeout(hover.hide);
  hover.hide = setTimeout(hideCard, 250);
}

function showCardFor(link, delay) {
  const rec = recordOf(link);
  if (!rec || link.closest("[data-ferrum-nocard]")) return;
  clearTimeout(hover.hide);
  if (hover.link === link) return;
  hideCard();
  hover.link = link;
  hover.timer = setTimeout(async () => {
    const ctl = new AbortController();
    hover.ctl = ctl;
    const slow = setTimeout(() => hover.link === link && drawCard(link, null), 300);
    const card = await cardFor(rec.entity, rec.id, ctl.signal).catch(() => null);
    clearTimeout(slow);
    if (hover.link !== link || ctl.signal.aborted) return;
    if (card) drawCard(link, card);
    else hideCard();
  }, delay);
}

if (boot?.person && hostKind !== "public") {
  document.addEventListener("pointerdown", (e) => {
    hover.touch = e.pointerType === "touch";
    if (!e.composedPath().includes(hover.host)) hideCard();
  });
  document.addEventListener("pointerover", (e) => {
    if (e.pointerType !== "mouse") return;
    const a = e.target instanceof Element ? e.target.closest("a[href]") : null;
    if (a) showCardFor(a, 450);
  });
  document.addEventListener("pointerout", (e) => {
    const a = e.target instanceof Element ? e.target.closest("a[href]") : null;
    if (a && a === hover.link && !(e.relatedTarget instanceof Node && a.contains(e.relatedTarget))) hideCardSoon();
  });
  document.addEventListener("focusin", (e) => {
    const a = e.target instanceof Element ? e.target.closest("a[href]") : null;
    if (a && !hover.touch) showCardFor(a, 600);
  });
  document.addEventListener("focusout", (e) => {
    if (e.target === hover.link) hideCard();
  });
  document.addEventListener("keydown", (e) => e.key === "Escape" && hideCard());
  addEventListener("scroll", () => hover.link && hideCard(), { capture: true, passive: true });
  addEventListener("pagehide", () => hideCard());
}

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
    const dialog = el("dialog", { "data-ferrum-dialog": "" });
    Object.assign(dialog.style, {
      // An app's reset (Tailwind's) takes away the margin browsers centre a dialog with.
      margin: "auto",
      padding: "16px",
      border: "1px solid var(--ferrum-border, #d9dee5)",
      borderRadius: "10px",
      background: "var(--ferrum-surface, #fff)",
      color: "var(--ferrum-text, #17191d)",
      boxShadow: "var(--ferrum-shadow)",
      font: "14px/1.4 var(--ferrum-font, system-ui, sans-serif)",
      minWidth: "300px",
    });
    dialog.setAttribute("aria-labelledby", "ferrum-choose-title");
    const always = el("input", { type: "checkbox", id: "ferrum-choose-always" });
    let picked = null;
    const buttons = handlers.map((h) =>
      el(
        "button",
        {
          type: "button",
          style: "display:flex;align-items:center;gap:10px;width:100%;text-align:left;margin:6px 0;padding:8px 10px;border:1px solid var(--ferrum-border,#d9dee5);border-radius:8px;background:transparent;color:inherit;font:inherit;cursor:pointer",
          onclick: () => {
            picked = h;
            dialog.close();
          },
        },
        tile((shellData?.apps ?? []).find((a) => a.app === h.app) ?? { title: appTitle_(h.app) }, true),
        el("span", { text: h.title ? `${appTitle_(h.app)} · ${h.title}` : appTitle_(h.app) }),
      ),
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
      const dialog = el("dialog", { "data-ferrum-dialog": "" });
      Object.assign(dialog.style, {
        margin: "auto",
        width: "min(880px, calc(100vw - 32px))",
        height: "min(640px, calc(100vh - 64px))",
        padding: "0",
        border: "1px solid var(--ferrum-border, #d9dee5)",
        borderRadius: "12px",
        overflow: "hidden",
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
    bar()?.setIndicator(`companion-${kind}`, companionsSeen.has(kind) ? `${title || kind} · Return` : "", () => companions?.postMessage({ type: "focus", kind }), "external-link");
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
  // A phone has no room beside the page: the panel's app instead (or the panel's own page).
  if (matchMedia("(max-width: 640px)").matches) {
    const entry = (data?.apps ?? []).find((a) => a.app === p.app);
    return navigate(entry?.url ?? p.url);
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
  const entry = (shellData?.apps ?? []).find((a) => a.app === p.app);
  const open = el("button", { type: "button", "aria-label": `Open ${p.title}`, title: `Open ${p.title}`, onclick: () => navigate(entry?.url ?? p.url) }, icon("external-link", 16));
  const close = el("button", { type: "button", "aria-label": `Close ${p.title}`, title: "Close", onclick: () => closeSidepanel() }, icon("x", 16));
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
    .map((a) => ({ id: a.id, app: a.app, title: a.title, url: a.url, icon: a.icon ?? null, color: a.color ?? null, current: under(a.url) }));
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
  b.setIndicator("uploads", text, () => toggleTray(), active.length ? "upload" : failed.length ? "circle-alert" : "check");
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
    const mark = u.state === "done" ? ["check", "ok"] : u.state === "failed" ? ["circle-alert", "err"] : u.state === "cancelled" ? ["x", null] : ["upload", null];
    return el(
      "li",
      {},
      el("div", { class: "row" }, el("span", { class: mark[1] ?? "mark" }, icon(mark[0], 15)), el("span", { class: "name", title: u.label, text: u.label }), ...actions.filter(Boolean)),
      FINISHED.has(u.state) ? null : el("progress", { max: String(u.size || 1), value: String(u.loaded ?? 0), "aria-label": `${u.label} sent` }),
      el("small", { class: u.state === "failed" ? "err" : null, text: u.state === "failed" && u.error ? `${state}: ${u.error}` : state }),
    );
  });
  const finished = list.filter((u) => FINISHED.has(u.state));
  b.tray.replaceChildren(
    el("h2", { text: "Uploads" }),
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

// Closing the last tab while a file is being sent asks first; moving to another page does not,
// unless the file is only in this page's memory (too large to keep, or a browser that keeps no
// files, such as Safari's private windows): then it ends with the page, wherever that goes.
// Listened for only while this page sends something: at other times the listener would keep
// pages out of Firefox's back/forward cache.
const onUploadLeave = (e) => {
  if (!running.size) return;
  if ([...running.keys()].some((id) => memoryFiles.has(id))) return e.preventDefault();
  if (Date.now() - leavingInSuite < 3000) return;
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

// A view transition between pages (theme.css) that the browser skips rejects its `ready`; nothing waits on it.
for (const type of ["pageswap", "pagereveal"]) window.addEventListener(type, (e) => e.viewTransition?.ready.catch(() => {}));

window.addEventListener("offline", () => bar()?.setIndicator("offline", "Offline", () => {}));
window.addEventListener("online", () => bar()?.setIndicator("offline", ""));

// ───────────────────────── the API ─────────────────────────

// A public page (§7.3) has no person: no intents, panels, uploads or live channel.
const PUBLIC_CAPABILITIES = ["navigate", "title", "toast", "theme", "locale", "ready"];
const CAPABILITIES = new Set(
  hostKind === "public"
    ? PUBLIC_CAPABILITIES
    : [...PUBLIC_CAPABILITIES, "breadcrumbs", "badge", "intent", "canIntent", "companion", "leaveGuard", "session", "live", "sidepanel", "uploads", "family", "assistant"],
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
    b?.drawApp?.();
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
  /** Opens the assistant beside this page with what the person is looking at (`{entity, id,
   *  title}`, a selection); false where there is none (the server has none, a guest). */
  assistant: (context) => sidepanel.open({ id: "assistant", context }),
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
