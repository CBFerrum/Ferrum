// Spike C3 (docs/plan/spike-c3-yjs.md): ten editors on one Yjs document through two replicas of
// spikes/yjs_server, with PostgreSQL behind them. Scenarios: steady editing, latency within and
// across replicas, a lost connection, a replica killed while people type, both replicas restarted.
// Every editor also appends to a log array, which nobody deletes from: its length proves that no
// change was lost.
//
//   npm i yjs@13.6.33 y-protocols@1.0.7 y-websocket@3.1.0 ws@8.21.3
//   SPIKE_BIN=…/yjs_server/target/release/yjs-spike node yjs_harness.mjs

import { spawn } from "node:child_process";
import { WebSocket } from "ws";
import { WebsocketProvider } from "y-websocket";
import * as Y from "yjs";

const BIN = process.env.SPIKE_BIN;
// Each provider listens for the process's exit.
process.setMaxListeners(50);
const PORTS = [7101, 7102];
const EDITORS = Number(process.env.EDITORS ?? 10);
const DOC = `spike-${Date.now().toString(36)}`;
const replicas = new Map();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const rand = (n) => Math.floor(Math.random() * n);
const words = ["alpha ", "beta ", "gamma ", "delta ", "ε ", "日本 ", "émoji 🎉 ", "\n"];
const out = (k, v) => console.log(`${k}: ${typeof v === "string" ? v : JSON.stringify(v)}`);
// Editors move by whole characters; SPLIT_PAIRS=1 lets edits fall inside an emoji's surrogate pair,
// which Yjs allows and a Rust string cannot hold.
const SPLIT = process.env.SPLIT_PAIRS === "1";
/** `i`, moved off the middle of a surrogate pair. */
const whole = (s, i) => (!SPLIT && i > 0 && i < s.length && /[\uDC00-\uDFFF]/.test(s[i]) ? i + 1 : i);

async function start(port) {
  const p = spawn(BIN, [String(port)], { stdio: ["ignore", "pipe", "pipe"] });
  p.stderr.on("data", (d) => process.stderr.write(`[${port}] ${d}`));
  replicas.set(port, p);
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`http://127.0.0.1:${port}/stats`)).ok) return;
    } catch {
      /* not up yet */
    }
    await sleep(100);
  }
  throw new Error(`replica ${port} did not start`);
}

async function kill(port) {
  const p = replicas.get(port);
  const gone = new Promise((r) => p.once("exit", r));
  p.kill("SIGKILL");
  await gone;
}

const server = (port) => fetch(`http://127.0.0.1:${port}/state/${DOC}`).then((r) => r.json());
const stats = (port) => fetch(`http://127.0.0.1:${port}/stats`).then((r) => r.json());

class Editor {
  constructor(i, port) {
    this.i = i;
    this.port = port;
    this.ops = 0;
    this.doc = new Y.Doc();
    this.text = this.doc.getText("body");
    this.log = this.doc.getArray("log");
    this.provider = new WebsocketProvider(`ws://127.0.0.1:${port}/docs`, DOC, this.doc, { WebSocketPolyfill: WebSocket, maxBackoffTime: 500 });
    this.provider.awareness.setLocalStateField("user", { name: `Editor ${i}` });
  }
  edit() {
    this.doc.transact(() => {
      const s = this.text.toString();
      const len = s.length;
      if (len > 600 && Math.random() < 0.45) {
        const at = whole(s, rand(len - 8));
        this.text.delete(at, whole(s, at + 1 + rand(8)) - at);
      } else this.text.insert(whole(s, rand(len + 1)), words[rand(words.length)]);
      this.log.push([`${this.i}:${this.ops++}`]);
    });
    this.provider.awareness.setLocalStateField("cursor", rand(this.text.length + 1));
  }
}

/** Every editor types every `every` ms (with jitter) for `ms`. */
async function type(editors, ms, every = 100) {
  const end = Date.now() + ms;
  await Promise.all(
    editors.map(async (e) => {
      while (Date.now() < end) {
        e.edit();
        await sleep(every / 2 + rand(every));
      }
    }),
  );
}

/** Waits until every editor and both replicas hold the same text and the full log. */
async function converge(editors, label, timeout = 30000) {
  const started = Date.now();
  const total = editors.reduce((n, e) => n + e.ops, 0);
  let last = {};
  while (Date.now() - started < timeout) {
    const texts = new Set(editors.map((e) => e.text.toString()));
    const logs = editors.map((e) => e.log.length);
    const states = await Promise.all(PORTS.filter((p) => replicas.get(p)?.exitCode === null).map(server)).catch(() => []);
    last = { distinct_texts: texts.size, logs_min: Math.min(...logs), total, servers: states.map((s) => ({ log: s.log, same_text: texts.has(s.text) })) };
    if (texts.size === 1 && logs.every((l) => l === total) && states.every((s) => s.log === total && texts.has(s.text))) {
      out(label, { converged_ms: Date.now() - started, ops: total, text_chars: [...texts][0].length });
      return true;
    }
    await sleep(100);
  }
  out(`${label} FAILED`, last);
  process.exitCode = 1;
  return false;
}

/** Milliseconds for a word typed by `from` to reach `to`, `rounds` times. */
async function latency(from, to, rounds = 30) {
  const times = [];
  for (let k = 0; k < rounds; k++) {
    const marker = `[m${k}-${from.i}]`;
    const seen = new Promise((resolve) => {
      const t0 = performance.now();
      const check = () => {
        if (to.text.toString().includes(marker)) {
          to.text.unobserve(check);
          resolve(performance.now() - t0);
        }
      };
      to.text.observe(check);
    });
    from.text.insert(0, marker);
    times.push(await seen);
    from.text.delete(from.text.toString().indexOf(marker), marker.length);
    await sleep(20);
  }
  times.sort((a, b) => a - b);
  return { median: +times[Math.floor(times.length / 2)].toFixed(1), p95: +times[Math.floor(times.length * 0.95)].toFixed(1), max: +times.at(-1).toFixed(1) };
}

const synced = (editors) => Promise.all(editors.map((e) => (e.provider.synced ? true : new Promise((r) => e.provider.once("sync", r)))));

try {
  await Promise.all(PORTS.map(start));
  out("document", DOC);
  const editors = Array.from({ length: EDITORS }, (_, i) => new Editor(i, PORTS[i % 2]));
  await synced(editors);
  await sleep(500);
  out("people each editor sees", [...new Set(editors.map((e) => e.provider.awareness.getStates().size))]);

  // 1. Steady: everyone types for 15 seconds, half of them on each replica.
  await type(editors, 15000);
  await converge(editors, `steady, ${EDITORS} editors on two replicas`);

  // 2. Latency: the same replica, and across replicas (through PostgreSQL and NOTIFY).
  out("latency on one replica (ms)", await latency(editors[0], editors[2]));
  out("latency across replicas (ms)", await latency(editors[0], editors[1]));

  // 3. A lost connection: one editor goes offline and keeps typing; so do the others.
  editors[3].provider.disconnect();
  await type(editors, 5000);
  editors[3].provider.connect();
  await converge(editors, "lost connection, offline edits merged");

  // 4. A replica killed while people type: its editors type offline until it is back.
  const typing = type(editors, 9000);
  await sleep(2000);
  await kill(PORTS[1]);
  out("replica killed", PORTS[1]);
  await sleep(4000);
  await start(PORTS[1]);
  out("replica restarted", PORTS[1]);
  await typing;
  await converge(editors, "replica restarted, nothing lost", 45000);

  out("replicas before the full restart", await Promise.all(PORTS.map(stats)));

  // 5. Both replicas restarted: a newcomer reads the document from PostgreSQL alone.
  const before = editors[0].text.toString();
  const total = editors.reduce((n, e) => n + e.ops, 0);
  for (const e of editors) e.provider.destroy();
  await Promise.all(PORTS.map(kill));
  await Promise.all(PORTS.map(start));
  const newcomer = new Editor(99, PORTS[0]);
  await synced([newcomer]);
  await sleep(300);
  out("after a full restart", { same_text: newcomer.text.toString() === before, log: newcomer.log.length, expected_log: total });
  const stored = await import("node:child_process").then(({ execFileSync }) =>
    execFileSync("psql", ["-h", "localhost", "-U", "ferrum", "-d", "yjs_spike", "-Atc",
      `select (select seq from spike_doc where id = '${DOC}') || ' changes, ' || (select count(*) from spike_update where doc = '${DOC}') || ' kept, snapshot of ' || coalesce((select length(data) from spike_snapshot where doc = '${DOC}'), 0) || ' bytes'`]).toString().trim(),
  ).catch(() => "unknown");
  out("stored", stored);
  if (newcomer.text.toString() !== before || newcomer.log.length !== total) process.exitCode = 1;
  newcomer.provider.destroy();
} catch (e) {
  out("FAILED", String(e?.stack ?? e));
  process.exitCode = 1;
} finally {
  for (const p of PORTS) if (replicas.get(p)?.exitCode === null) replicas.get(p).kill("SIGKILL");
  setTimeout(() => process.exit(), 200);
}
