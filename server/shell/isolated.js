// The page of an app whose interface runs isolated (docs/plan/apps-ui.md U8). The app is in the
// sandboxed frame #ferrum-app: an opaque origin, so no session, cookies or storage of this host,
// and a Content-Security-Policy that lets it reach its own files only. It reaches Ferrum through
// this bridge alone: screens' protocol 1 (docs/plugins.md "Screens": the host posts
// {ferrum: "init", version} with a MessagePort; requests {id, method, params} are answered with
// {id, result} or {id, error: {status, message}}; events go as {event, data}), with what a full
// page needs besides: where it is under its route, keeping the address bar in step, leaving for
// another page, its title and start-up, and short-lived app tokens. Calls to the API carry
// X-Ferrum-App, so the server narrows them to what the app declared, within the person's rights.

const PROTOCOL = 1;

function readBootstrap() {
  try {
    return JSON.parse(document.getElementById("ferrum-bootstrap")?.textContent ?? "null");
  } catch {
    return null;
  }
}

const boot = readBootstrap();
const app = boot?.app ?? {};
const frame = document.getElementById("ferrum-app");
// The route the page answers at, as `/board`.
const base = `/${String(app.entry ?? "").replace(/^\/+|\/+$/g, "")}`;
const shell = () => globalThis.ferrum?.shell;

/** Where the app is under its route: `/cards/42?tab=x`, `/` at its home. */
function here() {
  const rest = location.pathname === base || location.pathname.startsWith(`${base}/`) ? location.pathname.slice(base.length) : "/";
  return (rest || "/") + location.search;
}

/** The look, for the app to match: light or dark, and the theme's tokens as `--<name>` values. */
const TOKENS = ["bg", "text", "muted", "surface", "surface-2", "border", "primary", "primary-text", "primary-soft", "primary-hover", "danger", "radius", "shadow", "font", "focus-ring"];
function theme() {
  const root = document.documentElement;
  const style = getComputedStyle(root);
  const tokens = {};
  for (const t of TOKENS) {
    const v = style.getPropertyValue(`--ferrum-${t}`).trim();
    if (v) tokens[t] = v;
  }
  return { mode: root.dataset.theme === "dark" ? "dark" : "light", tokens };
}

class Refused extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

const text = (v, max) => (typeof v === "string" ? v.slice(0, max) : "");
const sitePath = (p) => typeof p === "string" && p.startsWith("/") && !p.startsWith("//") && !p.includes("\\") && !/[\u0000-\u001f]/.test(p);

/** Where the app may send requests: records, metadata and its own endpoints (the server checks the same). */
function mayCall(path) {
  if (/\.\.|[?#\\]|\/\//.test(path)) return false;
  return /^\/api\/(v1|meta)\/./.test(path) || path.startsWith(`/api/ext/${app.id}/`);
}

function queryString(q) {
  if (!q || typeof q !== "object" || Array.isArray(q)) return "";
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (["string", "number", "boolean"].includes(typeof v)) params.append(k, String(v));
  const s = params.toString();
  return s ? `?${s}` : "";
}

async function api(p) {
  const path = text(p.path, 2000);
  if (!mayCall(path)) throw new Refused(403, `${app.id} may not call ${path || "that"}`);
  const method = (text(p.method, 10) || "GET").toUpperCase();
  if (!["GET", "POST", "PATCH", "PUT", "DELETE"].includes(method)) throw new Refused(400, `unknown method ${method}`);
  const headers = { accept: "application/json", "x-ferrum-app": app.id };
  if (p.ifMatch !== undefined && p.ifMatch !== null) headers["if-match"] = `"${String(p.ifMatch).replace(/"/g, "")}"`;
  let body;
  if (p.body !== undefined && method !== "GET" && method !== "DELETE") {
    headers["content-type"] = "application/json";
    body = JSON.stringify(p.body);
  }
  const r = await fetch(path + queryString(p.query), { method, headers, body, credentials: "same-origin" });
  if (r.status === 401) {
    shell()?.session?.reauthenticate?.();
    throw new Refused(401, "signed out: sign in again");
  }
  const data = r.status === 204 ? null : await r.json().catch(() => null);
  if (!r.ok) throw new Refused(r.status, data?.fields?.[0]?.message ?? data?.message ?? `the request failed (${r.status})`);
  return data;
}

/** A short-lived token for the API as this app (docs/api.md, app tokens). */
async function token() {
  const r = await fetch("/api/shell/app-token", {
    method: "POST",
    credentials: "same-origin",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify({ app: app.id }),
  });
  const data = await r.json().catch(() => null);
  if (!r.ok) throw new Refused(r.status, data?.message ?? "no token");
  return data;
}

function confirmDialog({ title, message, confirmLabel, danger }) {
  return new Promise((resolve) => {
    const d = document.createElement("dialog");
    d.className = "ferrum-isolated-confirm";
    const h = document.createElement("h2");
    h.textContent = title;
    d.append(h);
    if (message) {
      const p = document.createElement("p");
      p.textContent = message;
      d.append(p);
    }
    const row = document.createElement("p");
    row.className = "ferrum-actions";
    const no = document.createElement("button");
    no.type = "button";
    no.className = "ferrum-button ferrum-button-quiet";
    no.textContent = "Cancel";
    const yes = document.createElement("button");
    yes.type = "button";
    yes.className = danger ? "ferrum-button ferrum-button-danger" : "ferrum-button";
    yes.textContent = confirmLabel || "OK";
    row.append(no, yes);
    d.append(row);
    const done = (v) => {
      d.close();
      d.remove();
      resolve(v);
    };
    no.addEventListener("click", () => done(false));
    yes.addEventListener("click", () => done(true));
    d.addEventListener("cancel", (e) => {
      e.preventDefault();
      done(false);
    });
    document.body.append(d);
    d.showModal();
    yes.focus();
  });
}

async function handle(method, p) {
  switch (method) {
    case "context":
      return {
        app: app.id,
        slot: "page",
        route: `${base}/`,
        path: here(),
        user: { email: boot?.person?.email ?? "", name: boot?.person?.name ?? "" },
        workspace: boot?.workspace?.name ?? "",
        locale: navigator.language,
        timezone: boot?.locale?.timezone ?? "",
        theme: theme(),
      };
    case "api":
      return api(p);
    case "token":
      return token();
    case "navigate": {
      // Where the app went inside its frame: the address bar follows, so a reload or a link comes back there.
      const to = text(p.to, 2000) || "/";
      if (!sitePath(to)) throw new Refused(400, "navigate to a path of the app, such as /cards/42");
      const url = `${base}${to === "/" ? "/" : to}`;
      if (p.replace === true) history.replaceState(null, "", url);
      else history.pushState(null, "", url);
      return null;
    }
    case "open": {
      const url = text(p.url, 2000);
      if (!sitePath(url)) throw new Refused(400, "open a page of this site, such as /open/Customer/42");
      location.assign(url);
      return null;
    }
    case "openRecord": {
      const [entity, id] = [text(p.entity, 100), text(p.id, 100)];
      if (!entity || !id) throw new Refused(400, "openRecord needs entity and id");
      location.assign(`/open/${encodeURIComponent(entity)}/${encodeURIComponent(id)}`);
      return null;
    }
    case "title": {
      const t = text(p.text, 200);
      shell()?.title?.(t);
      const name = app.title ?? app.id;
      if (t) document.title = t === name ? t : `${t} · ${name}`;
      return null;
    }
    case "toast": {
      const tone = p.tone === "error" || p.tone === "info" ? p.tone : "success";
      shell()?.toast?.(text(p.message, 300), { tone });
      return null;
    }
    case "confirm":
      return confirmDialog({
        title: text(p.title, 120) || "Are you sure?",
        message: text(p.message, 600) || undefined,
        confirmLabel: text(p.confirmLabel, 40) || undefined,
        danger: p.danger === true,
      });
    case "ready":
      shell()?.ready?.();
      return null;
    case "resize":
      // A full page fills the space below the bar.
      return null;
    default:
      throw new Refused(400, `unknown method ${method}`);
  }
}

let port = null;
const emit = (event, data) => port?.postMessage({ event, data });

function connect() {
  const win = frame?.contentWindow;
  if (!win) return;
  port?.close();
  const channel = new MessageChannel();
  port = channel.port1;
  port.onmessage = (e) => {
    const msg = e.data;
    if (!msg || typeof msg !== "object" || typeof msg.id !== "number" || typeof msg.method !== "string") return;
    const params = msg.params && typeof msg.params === "object" ? msg.params : {};
    handle(msg.method, params).then(
      (result) => port.postMessage({ id: msg.id, result: result ?? null }),
      (err) => port.postMessage({ id: msg.id, error: { status: err instanceof Refused ? err.status : 500, message: String(err?.message ?? err) } }),
    );
  };
  // The frame's origin is opaque, so "*": the port goes to whatever document it holds, which this
  // page's policy (frame-src) keeps to the app's own release.
  win.postMessage({ ferrum: "init", version: PROTOCOL }, "*", [channel.port2]);
}

frame?.addEventListener("load", connect);
// Back and forward move the app too.
window.addEventListener("popstate", () => emit("location", { path: here() }));
new MutationObserver(() => emit("theme", theme())).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme", "data-contrast"] });
