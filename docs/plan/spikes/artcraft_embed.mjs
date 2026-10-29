// Spike S3: GridCraft's web build embedded through its ?host=parent bridge, in headless Chrome.
//   node --experimental-websocket cdp-s3.mjs <ws-url> <out-dir>
import { writeFileSync } from "node:fs";

const [wsUrl, out] = process.argv.slice(2);
const ws = new WebSocket(wsUrl);
let seq = 0;
const waiting = new Map();
const problems = [];
ws.onmessage = (m) => {
  const msg = JSON.parse(m.data);
  if (msg.id && waiting.has(msg.id)) {
    const { resolve, reject } = waiting.get(msg.id);
    waiting.delete(msg.id);
    msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
  } else if (msg.method === "Runtime.exceptionThrown") {
    problems.push(msg.params.exceptionDetails.exception?.description?.slice(0, 200) ?? msg.params.exceptionDetails.text);
  } else if (msg.method === "Runtime.consoleAPICalled" && ["error", "warning"].includes(msg.params.type)) {
    problems.push(`${msg.params.type}: ${msg.params.args.map((a) => a.value ?? a.description ?? "").join(" ").slice(0, 200)}`);
  }
};
const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = ++seq;
    waiting.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const evaluate = async (expression) => (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true })).result.value;
const until = async (expression, ms) => {
  for (let t = 0; t < ms; t += 250) {
    if (await evaluate(expression)) return true;
    await sleep(250);
  }
  return false;
};
async function shot(name) {
  const { data } = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(`${out}/${name}.png`, Buffer.from(data, "base64"));
}
async function key(modifiers) {
  for (const type of ["keyDown", "keyUp"]) {
    await send("Input.dispatchKeyEvent", { type, modifiers, key: "s", code: "KeyS", windowsVirtualKeyCode: 83, nativeVirtualKeyCode: 83 });
  }
}
const report = {};

await new Promise((r) => (ws.onopen = r));
await send("Page.enable");
await send("Runtime.enable");
await send("Emulation.setDeviceMetricsOverride", { width: 1240, height: 800, deviceScaleFactor: 1, mobile: false });

// Same-origin parent: ready, open a workbook, save it back.
await send("Page.navigate", { url: "http://127.0.0.1:18771/host.html" });
report.ready = await until(`window.events?.some((e) => e.type === "gridcraft:ready")`, 45000);
report.events = await evaluate(`window.events`);
await evaluate(`openTest()`);
await sleep(3000);
await shot("s3-opened");
// Dismiss the "no styles part" notice (the test workbook is minimal), then focus the grid.
for (const [x, y] of [[776, 412], [300, 300]]) {
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "left", clickCount: 1 });
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "left", clickCount: 1 });
  await sleep(400);
}
await sleep(500);
for (const [label, modifiers] of [["ctrl", 2], ["meta", 4]]) {
  await key(modifiers);
  if (await until(`!!window.saved`, 5000)) {
    report.savedWith = label;
    break;
  }
}
report.saved = await evaluate(`window.saved && {name: saved.name, size: saved.size}`);
const b64 = await evaluate(`window.saved?.b64`);
if (b64) writeFileSync(`${out}/s3-saved.xlsx`, Buffer.from(b64, "base64"));
report.eventsAfter = (await evaluate(`window.events`)).map((e) => `${e.type}${e.dirty !== undefined ? "(dirty=" + e.dirty + ")" : ""}@${e.ms}ms`);
report.problems = problems.splice(0);

// The opaque-origin sandbox Ferrum's screens use.
await send("Page.navigate", { url: "http://127.0.0.1:18771/host-opaque.html" });
await sleep(15000);
report.opaque = { events: await evaluate(`window.events.map((e) => e.type + (e.error ? ": " + e.error : ""))`), problems: problems.splice(0, 6) };
await shot("s3-opaque");

console.log(JSON.stringify(report, null, 2));
ws.close();
