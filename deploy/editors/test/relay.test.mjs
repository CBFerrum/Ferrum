// The editor server's browser test (CI's editors job; docs/plan/browser-editors.md). It needs the
// server at EDITORS, started with FERRUM_PUBLIC_URL=$PARENT, and fixtures.py's site at PARENT:
//
//   docker run -d -p 8090:8080 -e FERRUM_PUBLIC_URL=http://127.0.0.1:8091 ferrum-editors
//   python3 fixtures.py site && python3 -m http.server 8091 --bind 127.0.0.1 -d site &
//   npm i playwright@1.64.1 && npx playwright install chromium firefox webkit
//   node relay.test.mjs        (ENGINES=chromium,firefox,webkit; CHANNEL=chrome: the installed Chrome)
//
// In each engine: each editor starts in the relay, opens its file, takes an edit and hands the
// file back through the bridge; the relay ignores pages it does not trust and editors it lacks;
// an editor cannot fetch from another origin. Before that, the server's headers.
import { inflateRawSync } from "node:zlib";
import { chromium, firefox, webkit } from "playwright";

const EDITORS = process.env.EDITORS ?? "http://127.0.0.1:8090";
const PARENT = process.env.PARENT ?? "http://127.0.0.1:8091";
// The same stand-in page from an origin the relay does not trust.
const STRANGER = PARENT.replace("127.0.0.1", "localhost");
const ENGINES = { chromium, firefox, webkit };
const failures = [];

function check(ok, what) {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
}

/** The text of each entry of a (small, non-ZIP64) zip archive. */
function unzip(buf) {
  const out = {};
  let end = buf.length - 22;
  while (end >= 0 && buf.readUInt32LE(end) !== 0x06054b50) end--;
  if (end < 0) return out;
  let p = buf.readUInt32LE(end + 16);
  for (let i = buf.readUInt16LE(end + 10); i > 0; i--) {
    const [method, size, nameLen, extra, comment, local] = [10, 20, 28, 30, 32, 42].map((o, k) => (k === 1 || k === 5 ? buf.readUInt32LE(p + o) : buf.readUInt16LE(p + o)));
    const name = buf.toString("utf8", p + 46, p + 46 + nameLen);
    const start = local + 30 + buf.readUInt16LE(local + 26) + buf.readUInt16LE(local + 28);
    const data = buf.subarray(start, start + size);
    out[name] = (method === 8 ? inflateRawSync(data) : data).toString("utf8");
    p += 46 + nameLen + extra + comment;
  }
  return out;
}

// program, file, the edit made after a click into the editor, and what the saved file must show.
const CASES = [
  [
    "gridcraft",
    "test.xlsx",
    async (page) => {
      // F2 opens the selected cell's editor, which takes a frame or two to take keys.
      await page.keyboard.press("F2");
      await page.waitForTimeout(600);
      await page.keyboard.type("Edited", { delay: 100 });
      await page.keyboard.press("Enter");
    },
    (bytes) => {
      const z = unzip(bytes);
      return (z["xl/workbook.xml"] ?? "").includes("Sales") && Object.values(z).some((x) => x.includes("Edited")) && Object.values(z).some((x) => x.includes("SUM(B2:B3)"));
    },
  ],
  [
    "wordcraft",
    "test.docx",
    async (page) => {
      await page.keyboard.type(" Edited", { delay: 100 });
    },
    (bytes) => (unzip(bytes)["word/document.xml"] ?? "").includes("Edited"),
  ],
  ["pdfcraft", "test.pdf", async () => {}, (bytes) => bytes.subarray(0, 5).toString() === "%PDF-"],
];

async function headers() {
  const get = (path, init) => fetch(EDITORS + path, init);
  const relay = await get("/relay.html");
  check(relay.ok && (relay.headers.get("content-security-policy") ?? "").includes(`frame-ancestors ${PARENT}`), "the relay may be framed by the web app only");
  const page = await get("/gridcraft/index.html");
  const csp = page.headers.get("content-security-policy") ?? "";
  check(
    page.ok && /script-src 'self' 'wasm-unsafe-eval' 'sha256-[A-Za-z0-9+/=]+'/.test(csp) && csp.includes("connect-src 'self'") && csp.includes("frame-ancestors 'self'"),
    "an editor's page runs only its own scripts and reaches only its origin",
  );
  check(page.headers.get("cache-control") === "no-cache", "an editor's page is revalidated");
  const wasm = (await page.text()).match(/[\w.-]+_bg\.wasm/)?.[0];
  const w = await get(`/gridcraft/${wasm}`, { method: "HEAD" });
  check(w.ok && w.headers.get("content-type") === "application/wasm" && /immutable/.test(w.headers.get("cache-control") ?? ""), "WebAssembly is served as such and cached for good");
  check(w.headers.get("x-content-type-options") === "nosniff" && w.headers.get("referrer-policy") === "no-referrer", "every file carries the common headers");
  const zipped = await get(`/gridcraft/${wasm}`, { method: "HEAD", headers: { "accept-encoding": "gzip" } });
  check(zipped.headers.get("content-encoding") === "gzip", "WebAssembly goes out compressed");
  check((await get("/healthz")).ok && (await get("/")).status === 404 && (await get("/gridcraft/../../etc/passwd")).status !== 200, "health answers; nothing else is served");
}

async function inEngine(name) {
  const options = name === "chromium" && process.env.CHANNEL ? { channel: process.env.CHANNEL } : {};
  // CI machines have no GPU: Chromium and WebKit draw WebGL in software. Headless Firefox offers no
  // WebGL2 there, so with a display (CI runs this under xvfb-run) it runs headed, on Mesa's software GL.
  if (name === "firefox") {
    options.firefoxUserPrefs = { "webgl.force-enabled": true, "webgl.disable-fail-if-major-performance-caveat": true };
    if (process.env.DISPLAY) options.headless = false;
  }
  const browser = await ENGINES[name].launch(options);
  try {
    const page = await (await browser.newContext({ viewport: { width: 1240, height: 800 } })).newPage();
    const open = (program, file, origin = PARENT) => page.goto(`${origin}/parent.html?${new URLSearchParams({ relay: `${EDITORS}/relay.html`, program, file })}`);
    const events = () => page.evaluate(() => window.events.map((e) => e.type + (e.error ? `:${e.error}` : "")));
    const until = async (fn, ms) => page.waitForFunction(fn, null, { timeout: ms }).then(() => true, () => false);
    const settled = () => until(() => window.events.some((e) => e.type === "ready" || e.type === "failed"), 60000);

    for (const [program, file, edit, saved] of CASES) {
      await open(program, file);
      await settled();
      const started = await events();
      check(started.includes("ready"), `${name}: ${program} starts (${started.join(", ") || "no answer"})`);
      if (!started.includes("ready")) continue;
      await page.waitForTimeout(3000);
      await page.frameLocator("#f").frameLocator("iframe").locator("canvas").click({ position: { x: 600, y: 380 } });
      await edit(page);
      let got = false;
      for (const keys of ["Control+s", "Meta+s"]) {
        await page.keyboard.press(keys);
        if ((got = await until(() => !!window.saved, 8000))) break;
      }
      check(got, `${name}: ${program} hands the file back through the bridge`);
      if (!got) continue;
      const s = await page.evaluate(() => window.saved);
      check(saved(Buffer.from(s.b64, "base64")), `${name}: ${program} saved the file with the edit (${s.name}, ${s.size} bytes)`);
    }

    // Untrusted pages, unknown editors, and an editor's reach.
    await open("gridcraft", "test.xlsx", STRANGER);
    await page.waitForTimeout(8000);
    check((await events()).length === 0, `${name}: a page on another origin gets nothing from the relay`);
    for (const [program, error] of [
      ["nothing", "missing"],
      ["../relay", "unknown"],
    ]) {
      await open(program, "");
      await settled();
      check((await events()).includes(`failed:${error}`), `${name}: the relay reports '${program}' as ${error}`);
    }
    await open("gridcraft", "");
    await settled();
    const frame = page.frames().find((f) => f.url().includes("/gridcraft/index.html"));
    const reached = await frame.evaluate((url) => fetch(url).then(() => true, () => false), `${PARENT}/test.xlsx`);
    check(!reached, `${name}: an editor cannot fetch from another origin`);
  } catch (e) {
    check(false, `${name}: ${e.message.split("\n")[0]}`);
  } finally {
    await browser.close();
  }
}

await headers();
for (const name of (process.env.ENGINES ?? "chromium,firefox,webkit").split(",")) await inEngine(name);
console.log(failures.length ? `\n${failures.length} failed` : "\nall passed");
process.exit(failures.length ? 1 : 0);
