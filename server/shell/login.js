// The sign-in page (/login) until the shell app brings its own: the workspace (where the host
// does not decide it), email and password, the second step for authenticator apps, and single
// sign-on. Signing in sets the browser's session cookie and goes back where the person was.

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

async function post(path, body) {
  const r = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(body),
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data?.error?.message ?? data?.message ?? `Signing in failed (${r.status})`);
  return data;
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
    say("This workspace requires two-factor sign-in. Set it up in the classic interface first.");
    const a = document.createElement("a");
    a.href = "/app/";
    a.textContent = "Open the classic interface";
    error.append(" ", a);
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
    if (ticket) {
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
        const { url } = await post("/api/auth/sso/start", { tenant, provider: p.id, redirect_uri: `${location.origin}/login`, binding });
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
