// The bar runtime's pure helpers (runtime.js, the block between "Pure helpers" and "end of the pure helpers"),
// run on their own with node's own test runner: `node --test server/shell/runtime.test.mjs`. The rest of the
// runtime needs a browser and is not tested here.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const src = readFileSync(new URL("./runtime.js", import.meta.url), "utf8");
const block = src.slice(src.indexOf("// ── Pure helpers"), src.indexOf("// ── end of the pure helpers"));
assert.ok(block.length > 100, "the block is where the markers say");
const { originStep, stillToSend, movedUrl, titleMarked } = new Function(`${block}; return { originStep, stillToSend, movedUrl, titleMarked };`)();

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
