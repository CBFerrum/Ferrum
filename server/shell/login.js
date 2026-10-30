// The sign-in page (/login) until the shell app brings its own: the workspace (where the host
// does not decide it), email and password, the second step for authenticator apps (set up here
// when the workspace requires one), and single sign-on. Signing in sets the browser's session
// cookie and goes back where the person was.

const boot = JSON.parse(document.getElementById("ferrum-login").textContent);
const form = document.getElementById("ferrum-login-form");
const fields = document.getElementById("ferrum-login-fields");
const error = document.getElementById("ferrum-login-error");
const submit = form.querySelector("button[type=submit]");
const store = {
  get(k) {
    try {
      return localStorage.getItem(k);
    } catch {
      return null;
    }
  },
  set(k, v) {
    try {
      localStorage.setItem(k, v);
    } catch {}
  },
};
let ticket = null;
// A workspace that requires two-factor sign-in, for someone who has not set it up: a token that
// may do only that; then, with the recovery codes shown, signing in again.
let setupToken = null;
let again = false;

function field(name, label, type, value = "", extra = {}) {
  const l = document.createElement("label");
  l.textContent = label;
  const input = document.createElement("input");
  Object.assign(input, { name, type, value, required: true, ...extra });
  l.append(input);
  fields.append(l);
  return input;
}

function workspace() {
  return boot.workspace?.slug ?? form.elements.workspace?.value.trim().toLowerCase() ?? "";
}

function say(message) {
  error.textContent = message;
}

async function post(path, body, headers = {}) {
  const r = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: { "content-type": "application/json", accept: "application/json", ...headers },
    body: JSON.stringify(body),
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data?.fields?.[0]?.message ?? data?.error?.message ?? data?.message ?? `Signing in failed (${r.status})`);
  return data;
}

function text(tag, content, className) {
  const e = document.createElement(tag);
  e.textContent = content;
  if (className) e.className = className;
  return e;
}

/** Two-factor sign-in on the way in: an authenticator app scans the code (or takes the key), and
 *  one of its codes turns it on. */
async function setUp(token) {
  setupToken = token;
  fields.replaceChildren();
  document.querySelector(".ferrum-login h1").textContent = "Set up two-factor sign-in";
  try {
    const begun = await post("/api/auth/2fa/begin", {}, { authorization: `Bearer ${token}` });
    const qr = Object.assign(document.createElement("img"), {
      src: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(begun.qr_svg)}`,
      alt: "QR code for your authenticator app",
      width: 200,
      height: 200,
      className: "ferrum-qr",
    });
    const link = Object.assign(document.createElement("a"), { href: begun.uri, textContent: "open it in the app" });
    const key = text("p", `Can't scan it? Enter ${begun.secret.replace(/(.{4})/g, "$1 ").trim()} in the app (time-based), or `, "ferrum-hint");
    key.append(link, ".");
    fields.append(
      text("p", "This workspace requires a code from an authenticator app at every sign-in. Scan this with one (Google Authenticator, Microsoft Authenticator, 1Password, Authy…), then enter the six-digit code it shows."),
      qr,
      key,
    );
    field("code", "Code from the app", "text", "", { autocomplete: "one-time-code", inputMode: "numeric" }).focus();
    submit.textContent = "Turn on";
  } catch (err) {
    setupToken = null;
    say(err.message);
  }
}

/** Once it is on: the recovery codes, shown this once, then signing in again with a code. */
function recoveryCodes(codes) {
  fields.replaceChildren(
    text("p", "Two-factor sign-in is on. Keep these recovery codes somewhere safe: each one signs you in once if you lose the app, and they are not shown again."),
    text("pre", codes.join("\n"), "ferrum-codes"),
  );
  submit.textContent = "Sign in";
  again = true;
}

function done(data) {
  if (data.second_factor) {
    ticket = data.ticket;
    fields.replaceChildren();
    const code = field("code", "Code from your authenticator app", "text", "", { autocomplete: "one-time-code", inputMode: "numeric" });
    const hint = document.createElement("p");
    hint.className = "ferrum-hint";
    hint.textContent = "Or one of your recovery codes.";
    fields.append(hint);
    code.focus();
    return;
  }
  if (data.two_factor_setup) {
    setUp(data.token);
    return;
  }
  if (data.cookie) {
    try {
      new BroadcastChannel("ferrum-session").postMessage({ type: "signed-in" });
    } catch {}
    location.replace(boot.next || "/");
    return;
  }
  say("This browser could not be signed in. Make sure cookies are allowed for this site.");
}

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  say("");
  submit.disabled = true;
  try {
    if (again) {
      location.reload();
      return;
    }
    if (setupToken) {
      const { recovery_codes } = await post("/api/auth/2fa/enable", { code: form.elements.code.value.trim() }, { authorization: `Bearer ${setupToken}` });
      setupToken = null;
      recoveryCodes(recovery_codes);
    } else if (ticket) {
      done(await post("/api/auth/login/second-factor", { ticket, code: form.elements.code.value.trim(), session: "cookie" }));
    } else {
      const tenant = workspace();
      if (!tenant) throw new Error("Which workspace?");
      store.set("ferrum.workspace", tenant);
      done(await post("/api/auth/login", { tenant, email: form.elements.email.value.trim(), password: form.elements.password.value, session: "cookie" }));
    }
  } catch (err) {
    say(err.message);
  } finally {
    submit.disabled = false;
  }
});

async function singleSignOn(tenant) {
  if (!tenant) return;
  const r = await fetch(`/api/auth/sso?tenant=${encodeURIComponent(tenant)}`, { credentials: "same-origin" }).catch(() => null);
  const providers = (await r?.json().catch(() => null))?.providers ?? [];
  document.getElementById("ferrum-sso")?.remove();
  if (!providers.length) return;
  const box = document.createElement("p");
  box.id = "ferrum-sso";
  box.className = "ferrum-actions";
  for (const p of providers) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "ferrum-button";
    b.textContent = `Sign in with ${p.name ?? p.label ?? "single sign-on"}`;
    b.addEventListener("click", async () => {
      try {
        const binding = crypto.randomUUID();
        sessionStorage.setItem("ferrum.sso", JSON.stringify({ binding, next: boot.next }));
        // The return address providers have registered (today's web app's); the server sends it on
        // to this page with the provider's answer.
        const { url } = await post("/api/auth/sso/start", { tenant, provider: p.id, redirect_uri: `${location.origin}/app/login`, binding });
        location.assign(url);
      } catch (err) {
        say(err.message);
      }
    });
    box.append(b);
  }
  form.after(box);
}

async function start() {
  const back = new URLSearchParams(location.search);
  const pending = sessionStorage.getItem("ferrum.sso");
  if (back.get("code") && back.get("state") && pending) {
    sessionStorage.removeItem("ferrum.sso");
    const { binding, next } = JSON.parse(pending);
    boot.next = next || boot.next;
    try {
      done(await post("/api/auth/sso/finish", { state: back.get("state"), code: back.get("code"), binding, session: "cookie" }));
      return;
    } catch (err) {
      say(err.message);
    }
  }
  if (!boot.workspace) {
    const ws = field("workspace", "Workspace", "text", store.get("ferrum.workspace") ?? "", { autocomplete: "organization", spellcheck: false });
    ws.addEventListener("change", () => singleSignOn(workspace()));
  }
  field("email", "Email", "email", "", { autocomplete: "username" });
  field("password", "Password", "password", "", { autocomplete: "current-password" });
  form.querySelector("input:not([value]), input[value='']")?.focus();
  singleSignOn(workspace());
}

start();
