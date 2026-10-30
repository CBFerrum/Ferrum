// The live channel (docs/plan/apps-ui.md §12.4): one /ws/live socket per browser and host, shared
// by every tab of every app through this SharedWorker (or one per tab, as a dedicated worker,
// where SharedWorker is missing). Pages subscribe to topics: entity names, whose changes arrive as
// {"event", "entity", "id", "actor"}, and "notification" and "push" (or "push:{kind}") for what
// the server sends a person without being asked. Only entity names go to the socket.
//
// Page → worker: {type: "subscribe" | "unsubscribe", topics: [...]}, {type: "bye"}.
// Worker → page: {type: "message", data}, {type: "denied", topics},
//                {type: "state", state: "open" | "closed" | "signed-out"}.

const ports = new Map(); // port → Set of topics
let socket = null;
let state = "closed";
let attempt = 0;
let timer = null;

const personal = (t) => t === "notification" || t === "push" || t.startsWith("push:");

function topicsWanted() {
  const all = new Set();
  for (const t of ports.values()) for (const x of t) if (!personal(x)) all.add(x);
  return all;
}

/** The topic a message from the server belongs to. */
function topicOf(data) {
  if (data.entity) return [data.entity];
  if (data.notification) return ["notification"];
  if (data.push) return ["push", `push:${data.push.kind ?? ""}`];
  return [];
}

function post(port, msg) {
  try {
    port.postMessage(msg);
  } catch {
    ports.delete(port);
  }
}

function broadcast(msg) {
  for (const port of [...ports.keys()]) post(port, msg);
}

function setState(s) {
  state = s;
  broadcast({ type: "state", state: s });
}

function connect() {
  clearTimeout(timer);
  if (socket || ports.size === 0) return;
  const url = new URL("/ws/live", self.location.origin);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  const ws = new WebSocket(url);
  socket = ws;
  ws.onmessage = (e) => {
    let data;
    try {
      data = JSON.parse(e.data);
    } catch {
      return;
    }
    if (data.ready) {
      attempt = 0;
      setState("open");
      const topics = [...topicsWanted()];
      if (topics.length) ws.send(JSON.stringify({ subscribe: topics }));
      return;
    }
    if (data.subscribed || data.denied) {
      const denied = Array.isArray(data.denied) ? data.denied : [];
      if (denied.length) for (const [port, topics] of ports) if (denied.some((t) => topics.has(t))) post(port, { type: "denied", topics: denied.filter((t) => topics.has(t)) });
      return;
    }
    const wanted = topicOf(data);
    for (const [port, topics] of ports) if (wanted.some((t) => topics.has(t))) post(port, { type: "message", data });
  };
  ws.onclose = (e) => {
    socket = null;
    if (e.code === 4401) {
      setState("signed-out");
      return;
    }
    setState("closed");
    // Jittered backoff: a replica restart or an upgrade does not bring every browser back at once.
    const delay = Math.min(30000, 1000 * 2 ** Math.min(attempt, 5)) * (0.5 + Math.random() / 2);
    attempt += 1;
    timer = setTimeout(connect, delay);
  };
}

function onMessage(port, msg) {
  if (!msg || typeof msg !== "object") return;
  if (msg.type === "bye") {
    ports.delete(port);
    if (ports.size === 0 && socket) socket.close(1000, "no pages");
    return;
  }
  const topics = ports.get(port) ?? new Set();
  ports.set(port, topics);
  const names = Array.isArray(msg.topics) ? msg.topics.filter((t) => typeof t === "string" && t.length < 100) : [];
  if (msg.type === "subscribe") {
    const fresh = names.filter((t) => !personal(t) && !topicsWanted().has(t));
    names.forEach((t) => topics.add(t));
    if (fresh.length && socket?.readyState === WebSocket.OPEN && state === "open") socket.send(JSON.stringify({ subscribe: fresh }));
  } else if (msg.type === "unsubscribe") {
    names.forEach((t) => topics.delete(t));
    const gone = names.filter((t) => !personal(t) && !topicsWanted().has(t));
    if (gone.length && socket?.readyState === WebSocket.OPEN && state === "open") socket.send(JSON.stringify({ unsubscribe: gone }));
  }
  post(port, { type: "state", state });
  connect();
}

function attach(port) {
  ports.set(port, new Set());
  port.onmessage = (e) => onMessage(port, e.data);
  if (port.start) port.start();
  post(port, { type: "state", state });
  connect();
}

if ("onconnect" in self) {
  self.onconnect = (e) => attach(e.ports[0]);
} else {
  // A dedicated worker: the page is the only "port".
  attach(self);
}
