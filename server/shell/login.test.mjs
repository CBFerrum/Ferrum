// login.js, run with node's own test runner (`node --test server/shell/login.test.mjs`) on a stand-in page and a
// stand-in server: a workspace that requires a second factor, reached by password, by an emailed code and at the
// accounts host. The setup step goes on with the cookie the browser holds (API-K2): no request carries
// `Authorization`; the emailed code is sent when the server says it mailed one; an ended setup sends the person back.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const src = readFileSync(new URL("./login.js", import.meta.url), "utf8");

class El {
  constructor(tag) {
    Object.assign(this, { tag, children: [], parent: null, listeners: {}, attrs: {}, textContent: "", value: "", hidden: false, disabled: false });
  }
  append(...kids) {
    for (const k of kids) {
      if (k instanceof El) k.parent = this;
      this.children.push(k);
    }
  }
  replaceChildren(...kids) {
    this.children = [];
    this.append(...kids);
  }
  after(e) {
    e.parent = this.parent;
    this.parent?.children.push(e);
  }
  remove() {
    if (this.parent) this.parent.children = this.parent.children.filter((c) => c !== this);
  }
  setAttribute(k, v) {
    this.attrs[k] = v;
  }
  addEventListener(type, f) {
    this.listeners[type] = f;
  }
  focus() {}
  *all() {
    for (const c of this.children) {
      if (c instanceof El) {
        yield c;
        yield* c.all();
      }
    }
  }
  querySelector(sel) {
    return sel === "button[type=submit]" ? this.submitButton : null;
  }
  get words() {
    return [this.textContent, ...[...this.all()].map((c) => c.textContent)].join(" ");
  }
}

function page(boot, answers) {
  const main = new El("main");
  const heading = new El("h1");
  heading.textContent = "Sign in";
  const form = new El("form");
  const fields = new El("div");
  const error = new El("p");
  const submit = new El("button");
  form.submitButton = submit;
  form.append(fields, error, submit);
  main.append(heading, form);
  Object.defineProperty(form, "elements", {
    get: () => Object.fromEntries([...fields.all()].filter((e) => e.tag === "input").map((e) => [e.name, e])),
  });
  const byId = { "ferrum-login": { textContent: JSON.stringify(boot) }, "ferrum-login-form": form, "ferrum-login-fields": fields, "ferrum-login-error": error, main };
  const requests = [];
  const went = [];
  const g = {};
  g.document = {
    getElementById: (id) => byId[id] ?? [...main.all()].find((e) => e.id === id) ?? null,
    querySelector: (sel) => (sel === ".ferrum-login h1" ? heading : null),
    createElement: (tag) => new El(tag),
  };
  g.window = {};
  g.localStorage = { getItem: () => null, setItem: () => {} };
  g.sessionStorage = { getItem: () => null, setItem: () => {}, removeItem: () => {} };
  g.location = { search: "", origin: "https://acme.test", assign: (u) => went.push(u), replace: (u) => went.push(u), reload: () => went.push("reload") };
  g.BroadcastChannel = class {
    postMessage() {}
  };
  g.fetch = async (path, init = {}) => {
    requests.push({ path, headers: init.headers ?? {}, body: init.body ? JSON.parse(init.body) : null });
    const [status, body] = answers(path.split("?")[0], requests.at(-1).body) ?? [200, {}];
    return { ok: status < 400, status, json: async () => body };
  };
  Object.assign(g, { URLSearchParams, setTimeout, console });
  return { g, form, fields, error, heading, requests, went };
}

const settle = () => new Promise((r) => setTimeout(r, 10));
/** The page's script, run in a context of its own on the stand-in page. */
async function load(p) {
  vm.runInNewContext(src, p.g);
  await settle();
}
async function press(p) {
  await p.form.listeners.submit({ preventDefault() {} });
  await settle();
}
function fill(p, values) {
  for (const [k, v] of Object.entries(values)) p.form.elements[k].value = v;
}
const SETUP = [200, { two_factor_setup: true, cookie: true, session_expires_in: 900 }];
const BEGUN = (mailed) => [200, { secret: "ABCDEFGHIJKLMNOP", uri: "otpauth://totp/x", qr_svg: "<svg/>", mailed }];

/** The setup step, then the recovery codes, then on to where the person was. */
async function setsUp(p, mailed) {
  assert.ok(p.form.elements.code, "the setup step asks for the app's code");
  assert.equal(Boolean(p.form.elements.mailed_code), mailed, "and for the emailed code when one was mailed");
  fill(p, mailed ? { code: "123456", mailed_code: "654321" } : { code: "123456" });
  await press(p);
  const enable = p.requests.find((r) => r.path === "/api/auth/2fa/enable");
  assert.deepEqual(enable.body, mailed ? { code: "123456", mailed_code: "654321" } : { code: "123456" });
  assert.match(p.fields.words, /recovery codes/);
  await press(p);
  assert.deepEqual(p.went, ["/next"], "on in, as after any sign-in");
  for (const r of p.requests) assert.ok(!Object.keys(r.headers).some((h) => h.toLowerCase() === "authorization"), `no Authorization on ${r.path}`);
}

const routes = (extra) => (path, body) =>
  ({ "/api/auth/2fa/begin": BEGUN(true), "/api/auth/2fa/enable": [200, { recovery_codes: ["aaaa-bbbb"] }], "/api/auth/sso": [200, { providers: [] }], ...extra })[path];

test("password sign-in reaches the setup step and sends the emailed code", async () => {
  const p = page({ workspace: { slug: "acme" }, next: "/next" }, routes({ "/api/auth/login": SETUP }));
  await load(p);
  fill(p, { email: "pat@acme.test", password: "Correct-Horse-9" });
  await press(p);
  await setsUp(p, true);
});

test("emailed-code sign-in reaches the setup step", async () => {
  const p = page({ workspace: { slug: "acme" }, codes: true, next: "/next" }, routes({ "/api/auth/code": [200, {}], "/api/auth/code/sign-in": SETUP, "/api/auth/2fa/begin": BEGUN(false) }));
  await load(p);
  fill(p, { email: "pat@acme.test" });
  await press(p);
  fill(p, { code: "111111" });
  await press(p);
  await setsUp(p, false);
});

test("the accounts host reaches the setup step", async () => {
  const p = page({ accounts: { mail: true }, next: "/next" }, routes({ "/api/accounts/sign-in": SETUP }));
  await load(p);
  fill(p, { email: "pat@acme.test", password: "Correct-Horse-9" });
  await press(p);
  await setsUp(p, true);
});

test("an ended setup sends the person back to sign in, saying so", async () => {
  const p = page({ workspace: { slug: "acme" }, next: "/next" }, routes({ "/api/auth/login": SETUP, "/api/auth/2fa/enable": [401, { error: { message: "sign in first" } }] }));
  await load(p);
  fill(p, { email: "pat@acme.test", password: "Correct-Horse-9" });
  await press(p);
  fill(p, { code: "123456", mailed_code: "654321" });
  await press(p);
  assert.ok(p.form.elements.password && !p.form.elements.code, "the sign-in form again");
  assert.match(p.error.textContent, /Sign in again/);
  assert.deepEqual(p.went, []);
});
