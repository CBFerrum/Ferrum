// The sign-in page (/login) until the shell app brings its own: the workspace (where the host
// does not decide it), email and password, the second step for authenticator apps (set up here
// when the workspace requires one), single sign-on, a code sent by email (the portal's way in,
// offered first on its way), and a passkey the device keeps (no email or password: the account
// is the passkey's). Signing in sets the browser's session cookie and goes back where the person was.
//
// On the accounts host (shapes B and C, `boot.accounts`) it signs in to the Ferrum account, once
// for every workspace: email and password (the first time, a workspace's password and then a code
// sent to the address), a password set with an emailed code, a passkey, the single sign-on of the
// workspace being entered, and the other regions' accounts hosts.

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
const accounts = boot.accounts ?? null;
// Accounts: the code sent to the address finishes making the account (its step's ticket), and
// the address a code went to, to set a password with.
let verifying = null;
let resetFor = null;
let typed = null;
// A workspace that requires two-factor sign-in, for someone who has not set it up: a token that
// may do only that; then, with the recovery codes shown, signing in again.
let setupToken = null;
let again = false;
// "code": signing in with an emailed code; `codeFor` is the address it went to.
let mode = accounts ? "password" : boot.codes ? "code" : "password";
const heading = document.querySelector(".ferrum-login h1");
const title = heading.textContent;
let codeFor = null;

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
  if (data.verify) {
    verifying = data.ticket;
    ticket = null;
    draw();
    return;
  }
  // A workspace's single sign-on, from the accounts host: back to the workspace's host.
  if (data.redirect) {
    location.assign(data.redirect);
    return;
  }
  if (data.second_factor) {
    ticket = data.ticket;
    fields.replaceChildren();
    for (const id of ["ferrum-switch", "ferrum-passkey", "ferrum-sso", "ferrum-regions"]) document.getElementById(id)?.remove();
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
    if (accounts) {
      await viaAccounts();
    } else if (setupToken) {
      const { recovery_codes } = await post("/api/auth/2fa/enable", { code: form.elements.code.value.trim() }, { authorization: `Bearer ${setupToken}` });
      setupToken = null;
      recoveryCodes(recovery_codes);
    } else if (ticket) {
      done(await post("/api/auth/login/second-factor", { ticket, code: form.elements.code.value.trim(), session: "cookie" }));
    } else if (mode === "code") {
      const tenant = workspace();
      if (!tenant) throw new Error("Which workspace?");
      store.set("ferrum.workspace", tenant);
      if (codeFor) {
        done(await post("/api/auth/code/sign-in", { tenant, email: codeFor, code: form.elements.code.value.trim(), session: "cookie" }));
      } else {
        const email = form.elements.email.value.trim();
        if (!email) throw new Error("Your email address?");
        await post("/api/auth/code", { tenant, email });
        codeFor = email;
        draw();
      }
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

/** A step of signing in at the accounts host. */
async function viaAccounts() {
  const code = () => form.elements.code.value.trim();
  if (ticket) return done(await post("/api/accounts/sign-in/second-factor", { ticket, code: code() }));
  if (verifying) return done(await post("/api/accounts/sign-in/verify", { ticket: verifying, code: code() }));
  if (resetFor) return done(await post("/api/accounts/password", { email: resetFor, code: code(), password: form.elements.password.value }));
  const email = form.elements.email.value.trim();
  if (!email) throw new Error("Your email address?");
  typed = email;
  if (mode === "reset") {
    await post("/api/accounts/code", { email });
    resetFor = email;
    draw();
    return;
  }
  done(await post("/api/accounts/sign-in", { email, password: form.elements.password.value }));
}

/** The accounts host's fields for the step at hand. */
function drawAccounts() {
  heading.textContent = title;
  const code = () => field("code", "Code from the email", "text", "", { autocomplete: "one-time-code", inputMode: "numeric", maxLength: 12 });
  if (accounts.sso_only && accounts.target) {
    submit.hidden = true;
    fields.append(text("p", `${accounts.target.name} signs its people in through its own single sign-on.`));
    singleSignOn(accounts.target.slug);
    return;
  }
  if (verifying) {
    heading.textContent = "Check your email";
    fields.append(text("p", `One more step to set up your Ferrum account: enter the code we sent to ${typed}. It works for 15 minutes.`, "ferrum-hint"));
    code().focus();
    submit.textContent = "Continue";
    return;
  }
  if (resetFor) {
    heading.textContent = "Set your password";
    fields.append(text("p", `If ${resetFor} has an account with us, a code is on its way to it. It works for 15 minutes.`, "ferrum-hint"));
    code().focus();
    field("password", "New password", "password", "", { autocomplete: "new-password", minLength: 10 });
    submit.textContent = "Set password and sign in";
    switcher("Use another address", () => {
      resetFor = null;
      draw();
    });
    return;
  }
  if (mode === "reset") {
    heading.textContent = "Set a password";
    fields.append(text("p", "Forgot your password, or never had one here? We will email you a code to set one.", "ferrum-hint"));
  }
  field("email", "Email", "email", typed ?? "", { autocomplete: "username webauthn" });
  if (mode === "reset") {
    submit.textContent = "Email me a code";
    switcher("Sign in with a password instead", () => {
      mode = "password";
      draw();
    });
  } else {
    field("password", "Password", "password", "", { autocomplete: "current-password" });
    submit.textContent = "Sign in";
    if (accounts.mail) {
      switcher("Forgot your password, or new here?", () => {
        mode = "reset";
        draw();
      });
    }
    if (accounts.target) singleSignOn(accounts.target.slug);
    passkeyButton();
  }
  if (accounts.regions?.length) {
    const p = text("p", "Your account is in another region? ", "ferrum-hint ferrum-regions");
    p.id = "ferrum-regions";
    accounts.regions.forEach((r, i) => {
      if (i) p.append(" · ");
      p.append(Object.assign(document.createElement("a"), { href: `${r.url}/login`, textContent: r.name.toUpperCase() }));
    });
    document.getElementById("main").append(p);
  }
  form.querySelector("input:not([value]), input[value='']")?.focus();
}

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
        // crypto.randomUUID exists on secure pages only.
        const binding = Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
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
      const answer = { state: back.get("state"), code: back.get("code"), binding };
      done(await post(accounts ? "/api/accounts/sso/finish" : "/api/auth/sso/finish", accounts ? { ...answer, next: boot.next } : { ...answer, session: "cookie" }));
      return;
    } catch (err) {
      say(err.message);
    }
  }
  draw();
}

/** The fields of the way in chosen: a password, or an emailed code (the address, then the code). */
function draw() {
  const kept = form.elements.workspace?.value;
  fields.replaceChildren();
  say("");
  document.getElementById("ferrum-switch")?.remove();
  document.getElementById("ferrum-sso")?.remove();
  document.getElementById("ferrum-passkey")?.remove();
  document.getElementById("ferrum-regions")?.remove();
  submit.hidden = false;
  if (accounts) return drawAccounts();
  if (mode === "code" && codeFor) {
    fields.append(text("p", `If ${codeFor} may sign in here, a code is on its way to it. It works for 10 minutes.`, "ferrum-hint"));
    field("code", "Code from the email", "text", "", { autocomplete: "one-time-code", inputMode: "numeric", maxLength: 12 }).focus();
    submit.textContent = "Sign in";
    switcher("Send another code, or use another address", () => {
      codeFor = null;
      draw();
    });
    return;
  }
  if (!boot.workspace) {
    const ws = field("workspace", "Workspace", "text", kept ?? store.get("ferrum.workspace") ?? "", { autocomplete: "organization", spellcheck: false });
    if (mode === "password") ws.addEventListener("change", () => singleSignOn(workspace()));
  }
  field("email", "Email", "email", "", { autocomplete: "username" });
  if (mode === "password") {
    field("password", "Password", "password", "", { autocomplete: "current-password" });
    submit.textContent = "Sign in";
    switcher("Email me a code instead", () => {
      mode = "code";
      draw();
    });
    singleSignOn(workspace());
  } else {
    submit.textContent = "Email me a code";
    switcher("Sign in with a password instead", () => {
      mode = "password";
      draw();
    });
  }
  passkeyButton();
  form.querySelector("input:not([value]), input[value='']")?.focus();
}

// Passkeys travel as base64url; the browser speaks ArrayBuffers.
const b64 = {
  decode: (s) => Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4)), (c) => c.charCodeAt(0)),
  encode: (buf) => btoa(String.fromCharCode(...new Uint8Array(buf))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, ""),
};

/** Signs in with a passkey on this device (or a phone nearby): the browser asks which one. */
async function passkey() {
  say("");
  const button = document.getElementById("ferrum-passkey");
  if (button) button.disabled = true;
  try {
    const { request, publicKey } = await post("/api/auth/passkey/options", {});
    const cred = await navigator.credentials.get({ publicKey: { ...publicKey, challenge: b64.decode(publicKey.challenge) } });
    if (!cred) return;
    const r = cred.response;
    const response = {
      clientDataJSON: b64.encode(r.clientDataJSON),
      authenticatorData: b64.encode(r.authenticatorData),
      signature: b64.encode(r.signature),
      userHandle: r.userHandle ? b64.encode(r.userHandle) : null,
    };
    done(await post("/api/auth/passkey", { request, session: "cookie", credential: { id: cred.id, rawId: b64.encode(cred.rawId), type: cred.type, response } }));
  } catch (e) {
    say(e?.name === "NotAllowedError" || e?.name === "AbortError" ? "No passkey was used." : e?.message || "Signing in with a passkey failed.");
  } finally {
    if (button) button.disabled = false;
  }
}

function passkeyButton() {
  if (!window.PublicKeyCredential || !window.isSecureContext) return;
  const b = document.createElement("button");
  b.type = "button";
  b.id = "ferrum-passkey";
  b.className = "ferrum-button ferrum-button-quiet";
  b.textContent = "Sign in with a passkey";
  b.addEventListener("click", () => void passkey());
  form.after(b);
}

function switcher(label, onclick) {
  const b = document.createElement("button");
  b.type = "button";
  b.id = "ferrum-switch";
  b.className = "ferrum-switch";
  b.textContent = label;
  b.addEventListener("click", onclick);
  form.after(b);
}

start();
