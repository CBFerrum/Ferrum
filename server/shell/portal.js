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

const CSS = `
:host { all: initial; color-scheme: inherit; display: block; position: sticky; top: 0; z-index: 2147483000; height: var(--ferrum-bar-height, 48px);
  font: 14px/1.4 var(--ferrum-font, system-ui, sans-serif); color: var(--ferrum-text, #17191d); -webkit-font-smoothing: antialiased; }
* { box-sizing: border-box; }
.bar { display: flex; align-items: center; gap: 4px; height: 100%; padding: 0 10px; background: var(--ferrum-surface, #fff);
  border-bottom: 1px solid var(--ferrum-border, #d9dee5); }
a, button { display: inline-flex; align-items: center; height: 34px; padding: 0 10px; border: 1px solid transparent; border-radius: 8px;
  background: transparent; color: inherit; font: inherit; text-decoration: none; white-space: nowrap; cursor: pointer; }
a:hover, button:hover { background: var(--ferrum-surface-2, #eef1f5); }
a:focus-visible, button:focus-visible { outline: none; box-shadow: var(--ferrum-focus-ring, 0 0 0 3px rgb(47 91 234 / .35)); }
.home { font-weight: 650; max-width: 16em; overflow: hidden; text-overflow: ellipsis; display: inline-block; line-height: 34px; }
nav { display: flex; gap: 2px; min-width: 0; overflow-x: auto; scrollbar-width: none; }
nav a { color: var(--ferrum-muted, #5b6472); }
nav a[aria-current="page"] { color: var(--ferrum-primary, #2f5bea); background: var(--ferrum-primary-soft, #e7edfd); }
.spacer { flex: 1; }
.who { color: var(--ferrum-muted, #5b6472); padding: 0 6px; max-width: 14em; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
@media (max-width: 640px) { .who { display: none; } .home { max-width: 9em; } }
`;

function el(tag, attrs = {}, text = "") {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
  if (text) e.textContent = text;
  return e;
}

async function signOut() {
  await fetch("/api/auth/logout", { method: "POST", credentials: "same-origin" }).catch(() => {});
  try {
    sessionStorage.removeItem("ferrum.session");
    sessionStorage.removeItem("ferrum.shell");
  } catch {}
  try {
    new BroadcastChannel("ferrum-session").postMessage({ type: "signed-out" });
  } catch {}
  location.assign("/login?next=%2Fportal%2F");
}

class PortalBar extends HTMLElement {
  connectedCallback() {
    if (this.shadowRoot) return;
    const root = this.attachShadow({ mode: "open" });
    const style = document.createElement("style");
    style.textContent = CSS;
    const portal = boot?.portal ?? { home: "/portal/", pages: [] };
    const bar = el("header", { class: "bar" });
    bar.append(el("a", { class: "home", href: portal.home ?? "/portal/" }, boot?.workspace?.name ?? "Portal"));
    const nav = el("nav", { "aria-label": "Your pages" });
    for (const p of portal.pages ?? []) {
      // Only this site's own addresses.
      if (typeof p.url !== "string" || !p.url.startsWith("/") || p.url.startsWith("//")) continue;
      const a = el("a", { href: p.url }, p.title ?? p.id ?? "");
      if (location.pathname.startsWith(p.url)) a.setAttribute("aria-current", "page");
      nav.append(a);
    }
    const out = el("button", { type: "button" }, "Sign out");
    out.addEventListener("click", signOut);
    bar.append(nav, el("span", { class: "spacer" }), el("span", { class: "who", title: boot?.person?.email ?? "" }, boot?.person?.name ?? ""), out);
    root.append(style, bar);
  }
}

customElements.define("ferrum-portal-bar", PortalBar);
