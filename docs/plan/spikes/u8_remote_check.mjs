// Checks the remote-rendering spike (docs/plan/spike-u8-remote-rendering.md) in Chrome: serve
// docs/plan/spikes/u8_remote on 127.0.0.1:8098 (python3 -m http.server 8098), then run
// `node u8_remote_check.mjs` with Playwright installed.
import { chromium } from "playwright";
const BASE = process.env.SPIKE_URL ?? "http://127.0.0.1:8098/";
const out = (k, v) => console.log(`${k}: ${typeof v === "string" ? v : JSON.stringify(v)}`);
const browser = await chromium.launch({ channel: process.env.CHANNEL ?? "chrome" });
const page = await browser.newPage();
let failed = false;
const expect = (ok, what) => {
  out(ok ? "ok" : "FAILED", what);
  if (!ok) failed = true;
};
try {
  await page.goto(BASE);
  await page.waitForFunction(() => Number(document.body.dataset.renders) >= 1);
  const log = () => page.locator("#log").innerText();
  const first = await log();
  expect(first.includes("parent document: unreachable") && first.includes("cookie: unreadable") && first.includes("storage: unreachable") && first.includes("origin: null"), "the extension reaches nothing of the host");
  expect(first.includes('refused an unknown component "Script"'), "unknown components are refused");
  expect(first.includes("refused a link to \"javascript:alert"), "links to elsewhere are refused");
  expect((await page.locator("#slot a[href='/helpdesk/t/42']").count()) === 1, "a link to one of the site's pages is drawn");
  await page.getByRole("button", { name: "4 of 5" }).click();
  await page.waitForFunction(() => Number(document.body.dataset.renders) >= 2);
  expect((await page.locator(".r-badge").innerText()) === "4 of 5", "an event goes to the extension and its new tree is drawn");
  await page.getByLabel("Anything to add?").fill("Quick help");
  await page.getByLabel("Anything to add?").blur();
  await page.waitForFunction(() => Number(document.body.dataset.renders) >= 3);
  await page.getByRole("button", { name: "Send" }).click();
  await page.getByRole("button", { name: "Thank you" }).waitFor();
  expect(true, "the form's flow completes through the host");
  // Larger trees: time to draw.
  for (const n of [100, 480]) {
    await page.goto(`${BASE}?big=${n}`);
    await page.waitForFunction(() => Number(document.body.dataset.renders) >= 1);
    out(`draw ${n} more nodes`, (await log()).match(/render 1 drawn in ([\d.]+) ms/)?.[1] + " ms");
  }
} catch (e) {
  failed = true;
  out("FAILED", e.message.split("\n")[0]);
}
await browser.close();
process.exit(failed ? 1 : 0);
