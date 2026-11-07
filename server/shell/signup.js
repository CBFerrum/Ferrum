// Making a workspace of one's own (/signup on the accounts host, FERRUM-CLOUD-002), where the
// operator takes sign-ups: an email address, a code sent to it, then the workspace's name, its
// address and a password. A new Ferrum account is signed in and goes to its workspaces; someone
// who had one signs in to it, and finds the new workspace there.

const boot = JSON.parse(document.getElementById("ferrum-signup").textContent);

// The page's words in the person's language: the workspace's own (Admin, Interface words) over the
// server's catalog of the shell's words (server/shell/locales), English where neither has one.
const WORDS = { ...(boot?.locale?.shell ?? {}), ...(boot?.locale?.words ?? {}) };
function tr(english, vars) {
  const text = WORDS[english] ?? english;
  return vars ? text.replace(/\{(\w+)\}/g, (whole, key) => (key in vars ? String(vars[key]) : whole)) : text;
}
const form = document.getElementById("ferrum-login-form");
const fields = document.getElementById("ferrum-login-fields");
const error = document.getElementById("ferrum-login-error");
const submit = form.querySelector("button[type=submit]");
const heading = document.querySelector(".ferrum-login h1");
let email = null;
let slugTouched = false;

function say(message) {
  error.textContent = message;
}

function text(tag, content, className) {
  const e = document.createElement(tag);
  e.textContent = content;
  if (className) e.className = className;
  return e;
}

function field(name, label, type, value = "", extra = {}) {
  const l = document.createElement("label");
  l.textContent = label;
  const input = document.createElement("input");
  Object.assign(input, { name, type, value, required: true, ...extra });
  l.append(input);
  fields.append(l);
  return input;
}

async function post(path, body) {
  const r = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(body),
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data?.fields?.[0] ? `${data.fields[0].field}: ${data.fields[0].message}` : (data?.error?.message ?? data?.message ?? tr("That did not work ({status})", { status: r.status })));
  return data;
}

/** A workspace's name as an address: lower-case letters, digits and hyphens. */
function slugOf(name) {
  return name
    .normalize("NFKD")
    .replace(/[̀-ͯ]/g, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 63);
}

function first() {
  heading.textContent = tr("Make your Ferrum workspace");
  fields.replaceChildren();
  if (boot.domains?.length) fields.append(text("p", tr("With an address at {domains}.", { domains: boot.domains.join(", ") }), "ferrum-hint"));
  field("email", tr("Your work email"), "email", email ?? "", { autocomplete: "email" }).focus();
  submit.textContent = tr("Email me a code");
}

function second() {
  heading.textContent = tr("Name your workspace");
  fields.replaceChildren(text("p", tr("We emailed a code to {email}. It works for 15 minutes.", { email }), "ferrum-hint"));
  field("code", tr("Code from the email"), "text", "", { autocomplete: "one-time-code", inputMode: "numeric", maxLength: 9 }).focus();
  const name = field("name", tr("Workspace name"), "text", "", { autocomplete: "organization", maxLength: 200 });
  const slug = field("slug", tr("Its address"), "text", "", { pattern: "[a-z0-9][a-z0-9-]{1,62}", maxLength: 63, spellcheck: false, autocapitalize: "off" });
  const where = text("p", "", "ferrum-hint");
  slug.closest("label").after(where);
  const show = () => (where.textContent = slug.value && boot.base ? `${slug.value}.${boot.base}` : "");
  name.addEventListener("input", () => {
    if (!slugTouched) slug.value = slugOf(name.value);
    show();
  });
  slug.addEventListener("input", () => {
    slugTouched = true;
    show();
  });
  field("full_name", tr("Your name"), "text", "", { autocomplete: "name", maxLength: 200 });
  field("password", tr("Password"), "password", "", { autocomplete: "new-password", minLength: 8 });
  fields.append(text("p", tr("You will be its administrator. You can add people and apps once you are in."), "ferrum-hint"));
  const back = Object.assign(document.createElement("button"), { type: "button", className: "ferrum-switch", textContent: tr("Use another address, or send the code again") });
  back.addEventListener("click", () => {
    say("");
    first();
  });
  fields.append(back);
  submit.textContent = tr("Make the workspace");
}

function made(data) {
  if (data.cookie) {
    location.assign("/");
    return;
  }
  heading.textContent = tr("Your workspace is ready");
  const p = text("p", tr("You already have a Ferrum account with this address: sign in with it, and the new workspace is in your list."));
  const a = Object.assign(document.createElement("a"), { className: "ferrum-button", href: "/login", textContent: tr("Sign in") });
  form.replaceWith(p, a);
}

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  say("");
  submit.disabled = true;
  try {
    const v = Object.fromEntries(new FormData(form));
    if (!email || v.email) {
      email = String(v.email ?? "").trim();
      await post("/api/accounts/signup/code", { email });
      second();
    } else {
      made(await post("/api/accounts/signup", { email, code: v.code, name: v.name, slug: v.slug, full_name: v.full_name, password: v.password }));
    }
  } catch (err) {
    say(err.message);
  } finally {
    submit.disabled = false;
  }
});

first();
