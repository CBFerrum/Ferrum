// The browser editors' fidelity run (docs/plan/browser-editors-fidelity.md): each corpus file
// opened in its editor through the relay, as Drive's editor page does, and saved unchanged with
// Ctrl/⌘+S; the saved bytes and a screenshot go to <out>. Needs the editor server on :8090 and
// deploy/editors/test's stand-in page, with the corpus beside it, on :8091 (relay.test.mjs says how).
//
//   node fidelity_run.mjs <corpus dir> <out dir> && python3 -I fidelity_compare.py <corpus dir> <out dir>
import { chromium } from "playwright";
import { readdirSync, writeFileSync } from "node:fs";
const [corpus, out] = process.argv.slice(2);
const program = { xlsx: "gridcraft", docx: "wordcraft", pdf: "pdfcraft" };
const b = await chromium.launch({ channel: "chrome" });
const report = {};
for (const file of readdirSync(corpus).sort()) {
  const ext = file.split(".").pop();
  const page = await (await b.newContext({ viewport: { width: 1240, height: 800 } })).newPage();
  const r = (report[file] = {});
  try {
    await page.goto(`http://127.0.0.1:8091/parent.html?${new URLSearchParams({ relay: "http://127.0.0.1:8090/relay.html", program: program[ext], file })}`);
    await page.waitForFunction(() => window.events.some((e) => e.type === "ready" || e.type === "failed"), null, { timeout: 60000 });
    await page.waitForTimeout(4000);
    r.events = await page.evaluate(() => window.events.map((e) => e.type + (e.error ? `:${e.error}` : "")));
    await page.frameLocator("#f").frameLocator("iframe").locator("canvas").click({ position: { x: 620, y: 420 } });
    for (const k of ["Control+s", "Meta+s"]) {
      await page.keyboard.press(k);
      if (await page.waitForFunction(() => !!window.saved, null, { timeout: 10000 }).then(() => true, () => false)) break;
    }
    const s = await page.evaluate(() => window.saved);
    if (s) { writeFileSync(`${out}/${file}`, Buffer.from(s.b64, "base64")); r.saved = s.size; }
    await page.screenshot({ path: `${out}/${file}.png` });
  } catch (e) { r.error = e.message.split("\n")[0]; }
  console.log(file, JSON.stringify(r));
  await page.close();
}
writeFileSync(`${out}/run.json`, JSON.stringify(report, null, 1));
await b.close();
