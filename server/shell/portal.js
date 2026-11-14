// The portal's header (docs/plan/apps-ui.md §9.1): portal pages carry <ferrum-portal-bar> instead
// of the bar. The workspace's name, the pages its apps give this person, and signing out; no
// launcher and no search, because portal people only ever see their own pages.

function readBootstrap() {
  try {
    return JSON.parse(document.getElementById("ferrum-bootstrap")?.textContent ?? "null");
  } catch {
    return null;
  }
}

const boot = readBootstrap();

// The page's words in the person's language: the workspace's own (Admin, Interface words) over the
// server's catalog of the shell's words (server/shell/locales), English where neither has one.
const WORDS = { ...(boot?.locale?.shell ?? {}), ...(boot?.locale?.words ?? {}) };
function tr(english, vars) {
  const text = WORDS[english] ?? english;
  return vars ? text.replace(/\{(\w+)\}/g, (whole, key) => (key in vars ? String(vars[key]) : whole)) : text;
}

const CSS = `
/* The suite's values (theme.css): 32px controls, 6px radius, hairline, 2px accent focus. */
:host { all: initial; color-scheme: inherit; display: block; position: sticky; top: 0; z-index: 2147483000; height: var(--ferrum-bar-height, 48px);
  font: 14px/20px var(--ferrum-font, system-ui, sans-serif); color: var(--ferrum-text, #202329); -webkit-font-smoothing: antialiased; }
* { box-sizing: border-box; }
.bar { position: relative; display: flex; align-items: center; gap: 4px; height: 100%; padding: 0 12px 0 8px; background: var(--ferrum-surface, #fff);
  border-bottom: 1px solid var(--ferrum-border, #dee3ea); }
a, button { display: inline-flex; align-items: center; height: var(--ferrum-control-h-sm, 32px); padding: 0 8px; border: 0; border-radius: var(--ferrum-radius-sm, 6px);
  background: transparent; color: inherit; font: inherit; text-decoration: none; white-space: nowrap; cursor: pointer;
  transition: background-color var(--ferrum-duration-state, 100ms), color var(--ferrum-duration-state, 100ms); }
a:hover, button:hover { background: var(--ferrum-surface-2, #eff1f5); }
a:active, button:active { background: var(--ferrum-surface-3, #e3e7ed); }
a:focus-visible, button:focus-visible { outline: var(--ferrum-focus-outline, 2px solid #3157d5); outline-offset: 2px; }
.home { font-weight: 600; max-width: 16em; overflow: hidden; text-overflow: ellipsis; display: inline-block; line-height: var(--ferrum-control-h-sm, 32px); }
nav { display: flex; gap: 4px; min-width: 0; overflow-x: auto; scrollbar-width: none; padding: 4px; margin: -4px; }
nav a { color: var(--ferrum-muted, #596273); }
nav a:hover { color: var(--ferrum-text, #202329); }
nav a[aria-current="page"] { color: var(--ferrum-primary, #3157d5); background: var(--ferrum-primary-soft, #e9eeff); }
.spacer { flex: 1; }
.who { color: var(--ferrum-muted, #596273); padding: 0 8px; max-width: 14em; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
button { color: var(--ferrum-muted, #596273); }
button:hover { color: var(--ferrum-text, #202329); }
.pages { display: none; }
.pages::after { content: ""; margin-inline-start: 8px; border: 4px solid transparent; border-top-color: currentColor; transform: translateY(2px); }
@media (prefers-reduced-motion: reduce) { a, button { transition: none; } }
/* A sandbox's strip, as on the bar (runtime.js): in the header's own height, never hidden. */
:host([sandbox]) .bar { height: calc(100% - 24px); }
.strip { display: flex; align-items: center; gap: 6px; height: 24px; padding: 0 12px; font-size: 12px; line-height: 16px; white-space: nowrap; overflow: hidden;
  background: var(--ferrum-warning-soft, #fff4d6); color: var(--ferrum-warning, #8f5700); border-bottom: 1px solid var(--ferrum-border, #dee3ea); }
.strip strong { flex: none; font-weight: 600; }
.strip span { overflow: hidden; text-overflow: ellipsis; min-width: 0; }
/* On a phone the pages do not fit beside each other: they open below a button naming the one shown. */
@media (max-width: 640px) {
  .who { display: none; } .home { max-width: 9em; }
  .pages { display: inline-flex; min-width: 0; }
  nav { display: none; position: absolute; top: calc(100% + 4px); inset-inline: 8px; flex-direction: column; gap: 0; padding: 4px; margin: 0; overflow: visible;
    background: var(--ferrum-surface, #fff); border: 1px solid var(--ferrum-border, #dee3ea); border-radius: var(--ferrum-radius, 8px); box-shadow: var(--ferrum-shadow); }
  nav.open { display: flex; }
  nav a { height: var(--ferrum-control-h, 36px); }
}
`;

function el(tag, attrs = {}, text = "") {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
  if (text) e.textContent = text;
  return e;
}

// The address the server gave after signing out (a SAML sign-in's single logout), only one of its own;
// the same check as runtime.js's afterSignOut.
async function signOut() {
  const r = await fetch("/api/auth/logout", { method: "POST", credentials: "same-origin" }).catch(() => null);
  const next = r?.status === 200 ? (await r.json().catch(() => null))?.next : null;
  try {
    sessionStorage.removeItem("ferrum.session");
    sessionStorage.removeItem("ferrum.shell");
  } catch {}
  try {
    new BroadcastChannel("ferrum-session").postMessage({ type: "signed-out" });
  } catch {}
  const saml = typeof next === "string" && /^\/api\/auth\/saml\/[0-9a-f-]{36}\/logout\?ticket=[A-Za-z0-9_-]+$/.test(next);
  location.assign(saml ? next : "/login?next=%2Fportal%2F");
}

class PortalBar extends HTMLElement {
  connectedCallback() {
    if (this.shadowRoot) return;
    const root = this.attachShadow({ mode: "open" });
    const style = document.createElement("style");
    style.textContent = CSS;
    const portal = boot?.portal ?? { home: "/portal/", pages: [] };
    const bar = el("header", { class: "bar" });
    bar.append(el("a", { class: "home", href: portal.home ?? "/portal/" }, boot?.workspace?.name ?? tr("Portal")));
    const nav = el("nav", { "aria-label": tr("Your pages"), id: "pages" });
    let current = null;
    for (const p of portal.pages ?? []) {
      // Only this site's own addresses.
      if (typeof p.url !== "string" || !p.url.startsWith("/") || p.url.startsWith("//")) continue;
      const a = el("a", { href: p.url }, p.title ?? p.id ?? "");
      if (location.pathname.startsWith(p.url)) {
        a.setAttribute("aria-current", "page");
        current = a.textContent;
      }
      nav.append(a);
    }
    const pages = el("button", { type: "button", class: "pages", "aria-controls": "pages", "aria-expanded": "false" }, current ?? tr("Pages"));
    const show = (open) => {
      pages.setAttribute("aria-expanded", String(open));
      nav.classList.toggle("open", open);
    };
    pages.addEventListener("click", () => show(pages.getAttribute("aria-expanded") !== "true"));
    bar.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        show(false);
        pages.focus();
      }
    });
    document.addEventListener("click", (e) => {
      if (!e.composedPath().includes(this)) show(false);
    });
    const out = el("button", { type: "button" }, tr("Sign out"));
    out.addEventListener("click", signOut);
    bar.append(pages, nav, el("span", { class: "spacer" }), el("span", { class: "who", title: boot?.person?.email ?? "" }, boot?.person?.name ?? ""), out);
    root.append(style, bar);
    if (boot?.sandbox) bar.after(this.sandboxStrip(boot.sandbox));
  }

  /** A sandbox's portal is where a tester acts as a customer: it says so as the bar does (runtime.js), with a
   *  strip the header grows by before the page below draws, and the page's title. */
  sandboxStrip(sb) {
    this.setAttribute("sandbox", "");
    const sheet = document.createElement("style");
    sheet.textContent = "@media screen { :root { --ferrum-bar-height: 72px; } }";
    document.head.append(sheet);
    const lang = boot?.locale?.language ?? undefined;
    const until = /^\d{4}-\d{2}-\d{2}$/.test(sb.until ?? "") ? new Date(`${sb.until}T00:00:00`).toLocaleDateString(lang, { day: "numeric", month: "long", year: "numeric" }) : String(sb.until ?? "");
    const text = tr("a copy of {name}. Nothing sent from here leaves the system. It is deleted after {until}.", { name: sb.name || sb.of || "", until });
    const mark = `${tr("Sandbox")} · `;
    const title = document.querySelector("title") ?? document.head.appendChild(document.createElement("title"));
    const keep = () => {
      if (!document.title.startsWith(mark)) document.title = `${mark}${document.title}`;
    };
    keep();
    new MutationObserver(keep).observe(title, { childList: true, characterData: true, subtree: true });
    const strip = el("div", { class: "strip", role: "note", title: text });
    strip.append(el("strong", {}, `${tr("Sandbox")}:`), el("span", {}, text));
    return strip;
  }
}

customElements.define("ferrum-portal-bar", PortalBar);
