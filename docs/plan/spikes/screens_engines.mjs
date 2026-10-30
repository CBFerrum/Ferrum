// Spike S4 (docs/plan/spike-s4-frames.md): apps' screens in Chromium, Firefox and WebKit.
//
// The board example (ferrum-sdk examples/board) installed in a workspace, the web app served at WEB
// with the API behind it. For each engine: the page, the widget on Home and the panel on a card's
// form load in their sandboxed frames and talk over the bridge (context, reads, a write, navigation);
// inside a frame, WebAssembly compiles and the app's own files can be fetched, while the parent page,
// storage and the API without the bridge are out of reach; a screen opened outside a frame is refused.
//
//   npm i playwright@1.64.1 && npx playwright install chromium firefox webkit
//   WEB=http://localhost:5175 API=http://127.0.0.1:5150 TENANT=demo EMAIL=admin@demo.test PASSWORD=… \
//     node screens_engines.mjs
import { chromium, firefox, webkit } from "playwright";

const WEB = process.env.WEB ?? "http://localhost:5175";
const API = process.env.API ?? "http://127.0.0.1:5150";
const { TENANT = "demo", EMAIL = "admin@demo.test", PASSWORD } = process.env;

const login = await fetch(`${API}/api/auth/login`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ tenant: TENANT, email: EMAIL, password: PASSWORD }),
});
const { token } = await login.json();
if (!token) throw new Error(`sign-in failed: HTTP ${login.status}`);
const session = JSON.stringify({ token, tenant: TENANT, email: EMAIL });

// An exported add(a, b) in the smallest WebAssembly module that has one.
const ADD_WASM = [0, 97, 115, 109, 1, 0, 0, 0, 1, 7, 1, 96, 2, 127, 127, 1, 127, 3, 2, 1, 0, 7, 7, 1, 3, 97, 100, 100, 0, 0, 10, 9, 1, 7, 0, 32, 0, 32, 1, 106, 11];

async function screenFrame(page, file) {
  for (let i = 0; i < 100; i++) {
    const f = page.frames().find((f) => f.url().includes("/api/ext-ui/") && f.url().endsWith(file));
    if (f) return f;
    await page.waitForTimeout(100);
  }
  throw new Error(`no frame for ${file}`);
}

async function inEngine(name, engine) {
  const checks = {};
  const errors = [];
  const browser = await engine.launch();
  try {
    const context = await browser.newContext();
    // Only the web app's own page holds the session; a frame's sessionStorage is not ours to touch.
    await context.addInitScript((s) => {
      if (window.top === window) {
        try {
          sessionStorage.setItem("ferrum.session", s);
        } catch {}
      }
    }, session);
    const page = await context.newPage();
    page.on("pageerror", (e) => errors.push(`page error: ${e.message}`));
    page.on("console", (m) => {
      if (m.type() === "error" && !/favicon|Failed to load resource: the server responded with a status of 40[13]/.test(m.text())) errors.push(m.text());
    });

    // The page: context, a read and a write over the bridge.
    await page.goto(`${WEB}/app/x/board/board`);
    const board = page.frameLocator('iframe[src*="/api/ext-ui/"]');
    await board.locator("#who").filter({ hasText: /\S/ }).waitFor({ timeout: 20000 });
    checks.context = (await board.locator("#who").textContent()).includes(EMAIL) || (await board.locator("#who").textContent()).length > 0;
    await board.locator("#cards li").first().waitFor();
    checks.read = !(await board.locator("#cards").textContent()).includes("Loading");
    const title = `S4 ${name} ${Date.now()}`;
    await board.locator("#title").fill(title);
    await board.locator("#add").click();
    await board.locator("#cards li", { hasText: title }).waitFor({ timeout: 15000 });
    checks.write = true;

    // Inside the frame: what its policy allows, and what it keeps out.
    const frame = await screenFrame(page, "board.html");
    const inside = await frame.evaluate(async (bytes) => {
      const out = {};
      try {
        const { instance } = await WebAssembly.instantiate(new Uint8Array(bytes));
        out.wasm = instance.exports.add(2, 3) === 5;
      } catch (e) {
        out.wasm = `refused: ${e.message}`;
      }
      try {
        const r = await fetch("board.js");
        out.ownFiles = r.ok && (await r.text()).includes("ferrum");
      } catch (e) {
        out.ownFiles = `refused: ${e.message}`;
      }
      try {
        const r = await fetch("/api/v1/BdCard");
        out.apiWithoutBridge = r.status;
      } catch (e) {
        out.apiWithoutBridge = "refused";
      }
      try {
        out.storage = typeof localStorage.length === "number" ? "open" : "blocked";
      } catch {
        out.storage = "blocked";
      }
      try {
        out.parent = parent.document.title ? "open" : "blocked";
      } catch {
        out.parent = "blocked";
      }
      out.origin = self.origin;
      return out;
    }, ADD_WASM);
    checks.wasm = inside.wasm;
    checks.ownFiles = inside.ownFiles;
    checks.apiWithoutBridge = inside.apiWithoutBridge === 401 || inside.apiWithoutBridge === "refused";
    checks.storageBlocked = inside.storage === "blocked";
    checks.parentBlocked = inside.parent === "blocked";
    checks.opaqueOrigin = inside.origin === "null";

    // The widget on Home, and the panel on the new card's form.
    await page.goto(`${WEB}/app/`);
    const widget = page.frameLocator('iframe[src*="count.html"]');
    await widget.locator("#total").filter({ hasText: /\d+ cards/ }).waitFor({ timeout: 20000 });
    checks.widget = true;
    const found = await fetch(`${API}/api/v1/BdCard?limit=1&filters=${encodeURIComponent(JSON.stringify({ title }))}`, {
      headers: { authorization: `Bearer ${token}` },
    }).then((r) => r.json());
    const card = found.rows[0].id;
    await page.goto(`${WEB}/app/e/BdCard/${card}`);
    await page.getByRole("tab", { name: "On the board" }).click();
    const panel = page.frameLocator('iframe[src*="panel.html"]');
    await panel.locator("#name", { hasText: title }).waitFor({ timeout: 20000 });
    checks.panel = true;
    await panel.locator("#open").click();
    await page.waitForURL(/\/app\/x\/board\/board/, { timeout: 15000 });
    checks.navigate = true;

    // A screen opened as a page of its own is refused.
    const direct = await page.goto(frame.url());
    checks.refusedOutsideFrame = direct.status() >= 400;

    // Tidy up the card made here.
    await fetch(`${API}/api/v1/BdCard/${card}`, { method: "DELETE", headers: { authorization: `Bearer ${token}` } });
  } catch (e) {
    checks.failure = e.message.split("\n")[0];
  } finally {
    await browser.close();
  }
  return { name, version: undefined, checks, errors };
}

const results = [];
for (const [name, engine] of [["chromium", chromium], ["firefox", firefox], ["webkit", webkit]]) {
  results.push(await inEngine(name, engine));
}
for (const r of results) {
  const failed = Object.entries(r.checks).filter(([, v]) => v !== true);
  console.log(`${r.name}: ${failed.length ? "FAIL " + JSON.stringify(Object.fromEntries(failed)) : "all checks pass"} (${Object.keys(r.checks).length} checks)`);
  for (const e of r.errors) console.log(`  console: ${e}`);
}
process.exit(results.every((r) => Object.values(r.checks).every((v) => v === true)) ? 0 : 1);
