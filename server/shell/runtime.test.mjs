// The bar runtime's pure helpers (runtime.js, the block between "Pure helpers" and "end of the pure helpers"),
// run on their own with node's own test runner: `node --test server/shell/runtime.test.mjs`. The rest of the
// runtime needs a browser and is not tested here.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const src = readFileSync(new URL("./runtime.js", import.meta.url), "utf8");
const block = src.slice(src.indexOf("// ── Pure helpers"), src.indexOf("// ── end of the pure helpers"));
assert.ok(block.length > 100, "the block is where the markers say");
const { originStep, stillToSend, movedUrl, marked, afterSignOut } = new Function(`${block}; return { originStep, stillToSend, movedUrl, marked, afterSignOut };`)();

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
  assert.equal(marked("Deals · CRM", "Sandbox · "), "Sandbox · Deals · CRM");
  assert.equal(marked("Sandbox · Deals · CRM", "Sandbox · "), "Sandbox · Deals · CRM");
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
