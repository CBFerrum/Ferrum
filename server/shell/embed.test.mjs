// embed.js, run with node's own test runner (`node --test server/shell/embed.test.mjs`) against a stand-in page: a
// window that dispatches messages, iframes, and a document that lists them.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const src = readFileSync(new URL("./embed.js", import.meta.url), "utf8");

function page(srcs) {
  const listeners = [];
  const events = [];
  const frames = srcs.map((s) => ({
    src: s,
    style: {},
    contentWindow: { name: s },
    matches: () => false,
    dispatchEvent: (ev) => events.push([s, ev.type, ev.bubbles]),
  }));
  const window = { addEventListener: (type, f) => type === "message" && listeners.push(f) };
  const document = {
    baseURI: "https://partner.example/page",
    documentElement: {},
    querySelectorAll: () => frames,
  };
  class MutationObserver {
    observe() {}
  }
  class CustomEvent {
    constructor(type, init) {
      this.type = type;
      this.bubbles = init?.bubbles;
    }
  }
  new Function("window", "document", "MutationObserver", "CustomEvent", src)(window, document, MutationObserver, CustomEvent);
  const post = (source, origin, data) => listeners.forEach((f) => f({ source, origin, data }));
  return { frames, events, post };
}

test("ignores_messages_from_other_sources_and_origins", () => {
  const { frames, events, post } = page(["https://my.acme.test/forms/f/acme/contact?embed=1", "https://my.globex.test/bookings/book/globex/"]);
  const [acme, globex] = frames;
  // Its own window and origin: sized, within bounds.
  post(acme.contentWindow, "https://my.acme.test", { ferrum: "height", height: 512.2 });
  assert.equal(acme.style.height, "513px");
  post(acme.contentWindow, "https://my.acme.test", { ferrum: "height", height: 1e9 });
  assert.equal(acme.style.height, "20000px");
  post(acme.contentWindow, "https://my.acme.test", { ferrum: "height", height: 3 });
  assert.equal(acme.style.height, "80px");
  // The right window from another origin, another frame's window with this origin, a third window: ignored.
  post(acme.contentWindow, "https://evil.example", { ferrum: "height", height: 700 });
  post(globex.contentWindow, "https://my.acme.test", { ferrum: "height", height: 700 });
  post({}, "https://my.acme.test", { ferrum: "height", height: 700 });
  assert.equal(acme.style.height, "80px");
  assert.equal(globex.style.height, undefined);
  // Not a number, or anything else: ignored.
  post(acme.contentWindow, "https://my.acme.test", { ferrum: "height", height: "900px" });
  post(acme.contentWindow, "https://my.acme.test", "<img src=x onerror=alert(1)>");
  assert.equal(acme.style.height, "80px");
  // Done: one event on its own frame, which bubbles to the site's page.
  post(globex.contentWindow, "https://my.globex.test", { ferrum: "done" });
  post(globex.contentWindow, "https://my.acme.test", { ferrum: "done" });
  assert.deepEqual(events, [["https://my.globex.test/bookings/book/globex/", "ferrum:done", true]]);
});
