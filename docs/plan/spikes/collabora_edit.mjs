// Spike S1: Collabora Online editing a Drive file through a WOPI host, in headless Chrome.
//   node --experimental-websocket cdp-s1.mjs <ws-url> <files-dir> <shots-dir>
import { readFileSync, writeFileSync, existsSync } from "node:fs";

const [wsUrl, files, shots] = process.argv.slice(2);
const ws = new WebSocket(wsUrl);
let seq = 0;
const waiting = new Map();
ws.onmessage = (m) => {
  const msg = JSON.parse(m.data);
  if (msg.id && waiting.has(msg.id)) {
    const { resolve, reject } = waiting.get(msg.id);
    waiting.delete(msg.id);
    msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
  }
};
const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = ++seq;
    waiting.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const calls = () => (existsSync(`${files}/wopi.log`) ? readFileSync(`${files}/wopi.log`, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse) : []);
const until = async (check, ms) => {
  for (let t = 0; t < ms; t += 500) {
    if (check()) return true;
    await sleep(500);
  }
  return false;
};
async function shot(name) {
  const { data } = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(`${shots}/${name}.png`, Buffer.from(data, "base64"));
}
async function click(x, y) {
  for (const type of ["mousePressed", "mouseReleased"]) await send("Input.dispatchMouseEvent", { type, x, y, button: "left", clickCount: 1 });
}
async function key(k, code, vk, modifiers) {
  for (const type of ["keyDown", "keyUp"]) await send("Input.dispatchKeyEvent", { type, modifiers, key: k, code, windowsVirtualKeyCode: vk });
}
const report = {};

await new Promise((r) => (ws.onopen = r));
await send("Page.enable");
await send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 860, deviceScaleFactor: 1, mobile: false });
const t0 = Date.now();
await send("Page.navigate", { url: "http://127.0.0.1:18900/editor.html" });
report.gotFile = await until(() => calls().some((c) => c.call === "GetFile"), 60000);
report.secondsToGetFile = (Date.now() - t0) / 1000;
await sleep(12000); // the editor renders its first tiles
await shot("s1-opened");
// Close the edition's welcome dialog, then put the cursor at the end of the first line.
await click(961, 205);
await sleep(1200);
await click(420, 245);
await sleep(500);
await key("End", "End", 35, 0);
await sleep(500);
await send("Input.insertText", { text: " Edited in Collabora through WOPI." });
await sleep(1500);
for (const [label, modifiers] of [["ctrl", 2], ["meta", 4]]) {
  await key("s", "KeyS", 83, modifiers);
  if (await until(() => calls().some((c) => c.call === "PutFile"), 10000)) {
    report.savedWith = label;
    break;
  }
}
await sleep(1500);
await shot("s1-saved");
report.calls = calls().map((c) => `${c.call}${c.proof ? " +proof" : ""}${c.lock ? " lock" : ""}`);
console.log(JSON.stringify(report, null, 2));
ws.close();
