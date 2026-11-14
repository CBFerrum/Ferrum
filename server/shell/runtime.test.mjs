// The bar runtime's pure helpers (runtime.js, the block between "Pure helpers" and "end of the pure helpers"),
// run on their own with node's own test runner: `node --test server/shell/runtime.test.mjs`. The rest of the
// runtime needs a browser and is not tested here.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const src = readFileSync(new URL("./runtime.js", import.meta.url), "utf8");
const block = src.slice(src.indexOf("// ── Pure helpers"), src.indexOf("// ── end of the pure helpers"));
assert.ok(block.length > 100, "the block is where the markers say");
const { originStep, stillToSend, movedUrl, titleMarked, afterSignOut, ownOrigin, isStepUp, replayPlan, stepUpGate, proveItsYou } = new Function(
  `${block}; return { originStep, stillToSend, movedUrl, titleMarked, afterSignOut, ownOrigin, isStepUp, replayPlan, stepUpGate, proveItsYou };`,
)();

test("the origin guard keeps a workspace's storage while its host is leaving, and wipes it for the next owner", () => {
  const acme = "0195-acme";
  // During the leaving period the old host still answers as the same workspace: nothing is wiped.
  assert.equal(originStep(acme, acme), "keep");
  assert.equal(originStep(acme, null), "remember");
  // A year on, another workspace claims the host: what the first left in this browser goes.
  assert.equal(originStep("0195-globex", acme), "wipe");
});

test("only the person's unsent uploads are still to send", () => {
  const me = "w/p";
  const list = [
    { owner: me, state: "queued" },
    { owner: me, state: "uploading" },
    { owner: me, state: "failed" },
    { owner: me, state: "done" },
    { owner: me, state: "cancelled" },
    { owner: "w/someone-else", state: "queued" },
  ];
  assert.equal(stillToSend(list, me), 3);
  assert.equal(stillToSend(list.filter((u) => u.state === "done"), me), 0);
  assert.equal(stillToSend([], me), 0);
});

test("the same page at the new address keeps to the new origin", () => {
  const loc = { pathname: "/pos/till", search: "?t=2", hash: "#sale" };
  assert.equal(movedUrl("https://acme.ferrum.cloud", loc), "https://acme.ferrum.cloud/pos/till?t=2#sale");
  assert.equal(movedUrl("http://acme.localhost:5150/", { pathname: "/", search: "", hash: "" }), "http://acme.localhost:5150/");
  // A path that looks like another host stays a path on the new one.
  assert.equal(new URL(movedUrl("https://new.example", { pathname: "//evil.example/x", search: "", hash: "" })).host, "new.example");
});

test("a sandbox's mark goes before the title once", () => {
  assert.equal(titleMarked("Deals · CRM", "Sandbox · "), "Sandbox · Deals · CRM");
  assert.equal(titleMarked("Sandbox · Deals · CRM", "Sandbox · "), "Sandbox · Deals · CRM");
});

// The server loads these as modules (`<script type="module">`), where a name declared twice at the top is a syntax
// error that stops the whole file: the bar then draws nothing on any page. Checking the file as a plain script, or
// only its pure helpers, does not see that, so each is parsed here the way a browser parses it.
test("each shell script parses as a module", async () => {
  const { execFileSync } = await import("node:child_process");
  const { mkdtempSync, copyFileSync, rmSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const { fileURLToPath } = await import("node:url");
  const dir = mkdtempSync(join(tmpdir(), "ferrum-shell-"));
  try {
    for (const name of ["runtime.js", "portal.js", "login.js", "assistant.js"]) {
      const copy = join(dir, name.replace(/\.js$/, ".mjs"));
      copyFileSync(fileURLToPath(new URL(`./${name}`, import.meta.url)), copy);
      try {
        execFileSync(process.execPath, ["--check", copy], { stdio: "pipe" });
      } catch (e) {
        assert.fail(`${name} does not parse as a module: ${String(e.stderr ?? e).split("\n").slice(0, 4).join(" ")}`);
      }
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("after signing out, the browser goes on only to a single-logout address the server gave", () => {
  const next = "/api/auth/saml/0195a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b/logout?ticket=Abc_123-x";
  assert.equal(afterSignOut(200, { next }, "/login"), next);
  assert.equal(afterSignOut(204, null, "/login"), "/login", "no SAML sign-out: the sign-in page");
  assert.equal(afterSignOut(undefined, null, "/login"), "/login", "the request failed");
  for (const other of ["https://evil.example/x", "//evil.example/api/auth/saml/x/logout?ticket=a", "/api/auth/saml/x/logout?ticket=a", `${next}&then=https://evil.example`, "javascript:alert(1)"]) {
    assert.equal(afterSignOut(200, { next: other }, "/login"), "/login", other);
  }
});

// ── Asking again on every page (the step-up refusal, docs/operations.md "Asking again before sensitive work") ──

test("only a 403 whose JSON says step_up is the refusal the runtime answers", () => {
  const asked = { error: "step_up", message: "confirm it is you: enter your password again to do this" };
  assert.equal(isStepUp(403, "application/json", asked), true);
  assert.equal(isStepUp(403, "application/json; charset=utf-8", asked), true);
  assert.equal(isStepUp(403, "application/json", { error: "forbidden", message: "not permitted" }), false, "a plain refusal");
  assert.equal(isStepUp(401, "application/json", asked), false, "a wrong password or an ended session");
  assert.equal(isStepUp(403, "text/html", asked), false);
  assert.equal(isStepUp(403, "application/json", null), false, "not JSON after all");
  assert.equal(isStepUp(403, null, asked), false);
});

test("only the page's own origin is wrapped", () => {
  const here = "https://acme.ferrum.cloud/crm/leads";
  assert.equal(ownOrigin("/api/v1/Lead", here), true);
  assert.equal(ownOrigin("https://acme.ferrum.cloud/api/v1/Lead?x=1", here), true);
  assert.equal(ownOrigin("https://crm.apps.ferrum.cloud/api/x", here), false, "an isolated app's origin");
  assert.equal(ownOrigin("//evil.example/api", here), false);
  assert.equal(ownOrigin("http://acme.ferrum.cloud/api", here), false, "another scheme is another origin");
  assert.equal(ownOrigin("http://[bad", here), false);
});

test("a refused request is sent again as it was, a Request's body included; a stream is not", async () => {
  const init = { method: "PATCH", body: JSON.stringify({ note: "x" }), headers: { "content-type": "application/json" } };
  const plan = replayPlan("/api/v1/Lead/1", init);
  assert.deepEqual(plan(), ["/api/v1/Lead/1", init], "fetch(url, init): the same arguments");
  const req = new Request("https://acme.test/api/v1/Lead", { method: "POST", body: JSON.stringify({ title: "new" }), headers: { "content-type": "application/json" } });
  const again = replayPlan(req, undefined);
  assert.equal(await req.text(), '{"title":"new"}', "the first send reads the request");
  const [copy, rest] = again();
  assert.equal(rest, undefined);
  assert.equal(copy.method, "POST");
  assert.equal(copy.headers.get("content-type"), "application/json");
  assert.equal(await copy.text(), '{"title":"new"}', "the copy taken before still has its body");
  assert.equal(replayPlan("/api/upload", { method: "POST", body: new ReadableStream(), duplex: "half" }), null, "a stream goes through untouched");
  assert.equal(replayPlan(req, undefined), null, "a request whose body was read cannot be copied");
});

test("many refusals at once ask once; refusals of requests sent before the proof are sent again without asking", async () => {
  let asked = 0;
  let answer;
  const gate = stepUpGate(() => {
    asked += 1;
    return new Promise((r) => (answer = r));
  });
  const before = gate.mark();
  const waiting = [gate.settle(before), gate.settle(before), gate.settle(before)];
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(asked, 1, "one dialog for three refusals");
  answer(true);
  assert.deepEqual(await Promise.all(waiting), [true, true, true], "each is sent again");
  assert.equal(await gate.settle(before), true, "sent before the proof, refused after it: sent again, not asked");
  assert.equal(asked, 1);
  // A later refusal asks again; cancelling it answers every waiting request with no.
  const later = gate.mark();
  const two = [gate.settle(later), gate.settle(later)];
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(asked, 2);
  answer(false);
  assert.deepEqual(await Promise.all(two), [false, false]);
  // A dialog that fails counts as a no, and the next refusal may ask again.
  const failing = stepUpGate(() => Promise.reject(new Error("no dialog")));
  assert.equal(await failing.settle(failing.mark()), false);
});

test("the proof is posted through the page's own fetch, and each answer says what the dialog shows", async () => {
  const sent = [];
  const answers = [];
  const send = async (url, init) => {
    sent.push([url, init.method, JSON.parse(init.body)]);
    const [status, body] = answers.shift();
    return new Response(body === null ? null : JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
  };
  answers.push([204, null]);
  assert.deepEqual(await proveItsYou(send, "Correct-Horse-9", ""), { ok: true });
  assert.deepEqual(sent.pop(), ["/api/auth/step-up", "POST", { password: "Correct-Horse-9" }], "no code unless asked");
  answers.push([401, { error: "unauthorized", message: "wrong password" }]);
  assert.deepEqual(await proveItsYou(send, "nope", ""), { ok: false, say: "wrong" }, "a wrong password: the dialog stays open");
  const needs = { error: "validation_failed", message: "Please check your input.", fields: [{ field: "code", message: "the code from your authenticator app is needed" }] };
  answers.push([422, needs]);
  assert.deepEqual(await proveItsYou(send, "Correct-Horse-9", ""), { ok: false, needsCode: true, say: "code" });
  answers.push([401, { error: "unauthorized", message: "invalid code" }]);
  assert.deepEqual(await proveItsYou(send, "Correct-Horse-9", "123456"), { ok: false, say: "wrong" });
  assert.deepEqual(sent.pop()[2], { password: "Correct-Horse-9", code: "123456" });
  answers.push([429, { error: "too_many_requests", message: "slow down" }]);
  assert.deepEqual(await proveItsYou(send, "x", ""), { ok: false, say: "busy" });
  assert.deepEqual(await proveItsYou(() => Promise.reject(new TypeError("offline")), "x", ""), { ok: false, say: "failed" });
});
