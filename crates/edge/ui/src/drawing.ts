// What a flow is, as the canvas sees it. The node kinds mirror `ferrum_ops_contract::flow`, which is
// what decides whether a flow is any good: this only helps a person draw one.

export interface FlowNode {
  id: string;
  kind: string;
  x: number;
  y: number;
  label?: string;
  [param: string]: unknown;
}

export interface FlowEdge {
  from: string;
  port?: string;
  to: string;
  input?: string;
}

export interface Flow {
  nodes: FlowNode[];
  edges: FlowEdge[];
}

export interface Param {
  name: string;
  label: string;
  type: "text" | "number" | "duration" | "select" | "check" | "json" | "list";
  hint?: string;
  options?: string[];
  required?: boolean;
  placeholder?: string;
}

export type Group = "Listen" | "Decide" | "Do";

export interface Kind {
  kind: string;
  label: string;
  group: Group;
  says: string;
  params: Param[];
  /** Ways out. A switch has one per case, so it works that out from its own settings. */
  ports: string[];
  /** Ways in, or null for inputs named by whoever wires them. */
  inputs: string[] | null;
  defaults?: Record<string, unknown>;
}

const device: Param = { name: "device", label: "Device", type: "text", placeholder: "Any device", hint: "A code, or the start of one and a star: PRESS-*" };
const yesNo = ["yes", "no"];
const out = ["out"];
const across: Param = { name: "across", label: "Across devices", type: "check", hint: "Otherwise each device is taken on its own" };
const fresh: Param = { name: "fresh", label: "Ignore values older than", type: "duration", placeholder: "Never", hint: "A stale reading is not a reading" };

export const KINDS: Kind[] = [
  {
    kind: "metric",
    label: "A number",
    group: "Listen",
    says: "A number a device reported",
    params: [{ name: "metric", label: "Value", type: "text", required: true, placeholder: "temp.c" }, device],
    ports: out,
    inputs: [],
    defaults: { metric: "" },
  },
  { kind: "state", label: "A state change", group: "Listen", says: "A device starting, stopping or going idle", params: [device], ports: out, inputs: [] },
  {
    kind: "event",
    label: "An event",
    group: "Listen",
    says: "Something a device said happened",
    params: [{ name: "name", label: "Event", type: "text", required: true }, device],
    ports: out,
    inputs: [],
    defaults: { name: "" },
  },
  {
    kind: "attribute",
    label: "An attribute",
    group: "Listen",
    says: "A setting or fact about a device changing",
    params: [{ name: "key", label: "Attribute", type: "text", required: true }, device],
    ports: out,
    inputs: [],
    defaults: { key: "" },
  },
  {
    kind: "timer",
    label: "A timer",
    group: "Listen",
    says: "Fires on its own, every so often",
    params: [{ name: "every", label: "Every", type: "duration", required: true }],
    ports: out,
    inputs: [],
    defaults: { every: "1m" },
  },
  {
    kind: "compare",
    label: "Compare",
    group: "Decide",
    says: "Sends it one way or the other",
    params: [
      { name: "op", label: "Is", type: "select", options: [">", ">=", "<", "<=", "==", "!="], required: true },
      { name: "value", label: "Than", type: "json", required: true, hint: "A number or some text" },
    ],
    ports: yesNo,
    inputs: ["in"],
    defaults: { op: ">", value: 0 },
  },
  {
    kind: "range",
    label: "In range",
    group: "Decide",
    says: "Between two numbers or outside them",
    params: [
      { name: "min", label: "From", type: "number", required: true },
      { name: "max", label: "To", type: "number", required: true },
    ],
    ports: yesNo,
    inputs: ["in"],
    defaults: { min: 0, max: 100 },
  },
  {
    kind: "hold",
    label: "Must last",
    group: "Decide",
    says: "Passes once it has lasted this long. One spike is not an alarm.",
    params: [{ name: "for", label: "For", type: "duration", required: true }],
    ports: out,
    inputs: ["in", "reset"],
    defaults: { for: "30s" },
  },
  {
    kind: "silence",
    label: "Gone quiet",
    group: "Decide",
    says: "Fires when nothing has arrived for this long",
    params: [{ name: "for", label: "For", type: "duration", required: true }],
    ports: out,
    inputs: ["in"],
    defaults: { for: "5m" },
  },
  {
    kind: "changed",
    label: "Only changes",
    group: "Decide",
    says: "Lets through what is different from last time",
    params: [{ name: "by", label: "By at least", type: "number", placeholder: "Any change" }],
    ports: out,
    inputs: ["in"],
  },
  {
    kind: "limit",
    label: "Not too often",
    group: "Decide",
    says: "At most one message this often",
    params: [{ name: "every", label: "Every", type: "duration", required: true }],
    ports: out,
    inputs: ["in"],
    defaults: { every: "1m" },
  },
  {
    kind: "delay",
    label: "Wait",
    group: "Decide",
    says: "Passes each message on later",
    params: [{ name: "for", label: "For", type: "duration", required: true }],
    ports: out,
    inputs: ["in"],
    defaults: { for: "10s" },
  },
  {
    kind: "count",
    label: "Count",
    group: "Decide",
    says: "Passes when this many have arrived in a time",
    params: [
      { name: "of", label: "How many", type: "number", required: true },
      { name: "within", label: "Within", type: "duration", required: true },
    ],
    ports: out,
    inputs: ["in"],
    defaults: { of: 3, within: "1m" },
  },
  {
    kind: "window",
    label: "Average over time",
    group: "Decide",
    says: "An average, highest, lowest or total of the last while",
    params: [
      { name: "stat", label: "Work out", type: "select", options: ["avg", "min", "max", "sum", "count", "delta"], required: true },
      { name: "over", label: "Over", type: "duration", required: true },
    ],
    ports: out,
    inputs: ["in"],
    defaults: { stat: "avg", over: "1m" },
  },
  {
    kind: "math",
    label: "Calculate",
    group: "Decide",
    says: "Works something out from the value",
    params: [{ name: "formula", label: "Formula", type: "text", required: true, placeholder: "[value] * 1.8 + 32", hint: "+ - * / round abs min max" }],
    ports: out,
    inputs: ["in"],
    defaults: { formula: "[value]" },
  },
  {
    kind: "combine",
    label: "Combine",
    group: "Decide",
    says: "Waits for every input, then passes them together",
    params: [{ name: "formula", label: "Formula", type: "text", placeholder: "[a] * [b]", hint: "Inputs by the names on their wires" }, fresh, across],
    ports: out,
    inputs: null,
  },
  {
    kind: "all",
    label: "All true",
    group: "Decide",
    says: "Yes when every input is true, and no when that stops",
    params: [{ name: "any", label: "One is enough", type: "check" }, fresh, across],
    ports: yesNo,
    inputs: null,
  },
  { kind: "latch", label: "Remember", group: "Decide", says: "Stays on from set until reset", params: [], ports: yesNo, inputs: ["set", "reset"] },
  {
    kind: "map",
    label: "Translate",
    group: "Decide",
    says: "What a raw value means: 0 is Idle, 1 is Running",
    params: [
      { name: "map", label: "Meanings", type: "json", required: true, hint: '{"0": "Idle", "1": "Running"}' },
      { name: "other", label: "Otherwise", type: "json", placeholder: "Drop it" },
    ],
    ports: out,
    inputs: ["in"],
    defaults: { map: {} },
  },
  {
    kind: "switch",
    label: "Choose by value",
    group: "Decide",
    says: "One way out for each value",
    params: [{ name: "cases", label: "Values", type: "list", required: true, hint: "One to a line" }],
    ports: ["other"],
    inputs: ["in"],
    defaults: { cases: ["Running", "Stopped"] },
  },
  {
    kind: "set",
    label: "Replace",
    group: "Decide",
    says: "Passes something else instead",
    params: [{ name: "value", label: "With", type: "json", required: true, hint: "{value} and {device} work here" }],
    ports: out,
    inputs: ["in"],
    defaults: { value: "" },
  },
  {
    kind: "field",
    label: "Take a part",
    group: "Decide",
    says: "One thing out of a value with parts",
    params: [{ name: "path", label: "Path", type: "text", required: true, placeholder: "$.a.b" }],
    ports: out,
    inputs: ["in"],
    defaults: { path: "" },
  },
  {
    kind: "alarm",
    label: "Raise an alarm",
    group: "Do",
    says: "Raises at the top input and clears at the other",
    params: [
      { name: "name", label: "Name", type: "text", required: true },
      { name: "severity", label: "Severity", type: "select", options: ["Info", "Warning", "Critical"], required: true },
      { name: "device", label: "On device", type: "text", placeholder: "The one it came from" },
    ],
    ports: [],
    inputs: ["in", "clear"],
    defaults: { name: "", severity: "Warning" },
  },
  {
    kind: "notify",
    label: "Tell someone",
    group: "Do",
    says: "Tells whoever looks after IoT Edge",
    params: [
      { name: "title", label: "Message", type: "text", required: true, hint: "{value} and {device} work here" },
      { name: "every", label: "At most every", type: "duration" },
    ],
    ports: [],
    inputs: ["in"],
    defaults: { title: "" },
  },
  {
    kind: "command",
    label: "Ask for a command",
    group: "Do",
    says: "Asks the ledger. It waits for a person unless an automation envelope allows it.",
    params: [
      { name: "operation", label: "Operation", type: "text", required: true, placeholder: "fan.start" },
      { name: "device", label: "Device", type: "text", placeholder: "The one it came from" },
      { name: "payload", label: "With", type: "json", hint: "{value} and {device} work here" },
      { name: "every", label: "At most every", type: "duration" },
      { name: "expires", label: "Gives up after", type: "duration" },
    ],
    ports: [],
    inputs: ["in"],
    defaults: { operation: "" },
  },
  { kind: "record_event", label: "Record an event", group: "Do", says: "Writes it on the device's history", params: [{ name: "name", label: "Event", type: "text", required: true }], ports: [], inputs: ["in"], defaults: { name: "" } },
  {
    kind: "record_metric",
    label: "Record a number",
    group: "Do",
    says: "A new value of the device's, as if it had said it",
    params: [
      { name: "name", label: "Value", type: "text", required: true },
      { name: "unit", label: "Unit", type: "text" },
    ],
    ports: [],
    inputs: ["in"],
    defaults: { name: "" },
  },
  { kind: "record_attribute", label: "Set an attribute", group: "Do", says: "Writes down a fact about the device", params: [{ name: "key", label: "Attribute", type: "text", required: true }], ports: [], inputs: ["in"], defaults: { key: "" } },
  { kind: "export", label: "Send elsewhere", group: "Do", says: "Hands it to one of the site's exports, kept until delivered", params: [{ name: "to", label: "Export", type: "text", required: true }], ports: [], inputs: ["in"], defaults: { to: "" } },
];

const byKind = new Map(KINDS.map((k) => [k.kind, k]));

export const kindOf = (kind: string): Kind => byKind.get(kind) ?? { kind, label: kind, group: "Decide", says: "", params: [], ports: out, inputs: ["in"] };

export const portsOf = (n: FlowNode): string[] => (n.kind === "switch" ? [...((n.cases as string[]) ?? []), "other"] : kindOf(n.kind).ports);

/** Ways in that exist on the canvas: a combine's are whatever has been wired to it, and one more. */
export function inputsOf(flow: Flow, n: FlowNode): string[] {
  const fixed = kindOf(n.kind).inputs;
  if (fixed) return fixed;
  return [...new Set(flow.edges.filter((e) => e.to === n.id).map((e) => e.input ?? "in"))];
}

/** A name for one more wire into a step whose inputs are named by their wires. */
export function nextInput(flow: Flow, n: FlowNode): string {
  const used = inputsOf(flow, n);
  return "abcdefghijklmnopqrstuvwxyz".split("").find((c) => !used.includes(c)) ?? `in${used.length}`;
}

/** Where a wire goes when someone joins it to a step, and not to one of its dots. */
export function firstInput(flow: Flow, n: FlowNode): string | null {
  const fixed = kindOf(n.kind).inputs;
  if (fixed === null) return nextInput(flow, n);
  return fixed[0] ?? null;
}

export const NODE_W = 184;
export const NODE_H = 62;
const COLUMN = 244;
const ROW = 96;

/** Where a way in or out sits on a node. */
export function portAt(n: FlowNode, side: "in" | "out", index: number, count: number): { x: number; y: number } {
  return { x: n.x + (side === "out" ? NODE_W : 0), y: n.y + (NODE_H * (index + 1)) / (count + 1) };
}

/** A wire, as a curve that leaves and arrives level. */
export function wirePath(a: { x: number; y: number }, b: { x: number; y: number }): string {
  const bend = Math.max(36, Math.abs(b.x - a.x) / 2);
  return `M${a.x},${a.y} C${a.x + bend},${a.y} ${b.x - bend},${b.y} ${b.x},${b.y}`;
}

export function reaches(flow: Flow, from: string, to: string): boolean {
  const seen = new Set<string>();
  const walk = [from];
  while (walk.length) {
    const here = walk.pop()!;
    if (here === to) return true;
    if (seen.has(here)) continue;
    seen.add(here);
    walk.push(...flow.edges.filter((e) => e.from === here).map((e) => e.to));
  }
  return false;
}

/** Why this wire cannot be made, in words, or null. The server has the last word on all of this. */
export function whyNot(flow: Flow, e: FlowEdge): string | null {
  if (e.from === e.to) return "A step cannot lead to itself.";
  if (reaches(flow, e.to, e.from)) return "That would go round in a circle, and a flow cannot.";
  if (flow.edges.some((x) => x.from === e.from && (x.port ?? "out") === (e.port ?? "out") && x.to === e.to && (x.input ?? "in") === (e.input ?? "in"))) return "They are already joined that way.";
  return null;
}

export const connect = (flow: Flow, e: FlowEdge): Flow => (whyNot(flow, e) ? flow : { ...flow, edges: [...flow.edges, e] });

export const disconnect = (flow: Flow, e: FlowEdge): Flow => ({ ...flow, edges: flow.edges.filter((x) => x !== e) });

export function remove(flow: Flow, id: string): Flow {
  return { nodes: flow.nodes.filter((n) => n.id !== id), edges: flow.edges.filter((e) => e.from !== id && e.to !== id) };
}

export function change(flow: Flow, id: string, patch: Record<string, unknown>): Flow {
  const nodes = flow.nodes.map((n) => {
    if (n.id !== id) return n;
    const next: FlowNode = { ...n, ...patch };
    for (const [k, v] of Object.entries(patch)) if (v === undefined || v === "") delete next[k];
    return next;
  });
  // A case that went away takes its wires with it.
  const node = nodes.find((n) => n.id === id);
  const ports = node ? portsOf(node) : [];
  const edges = node?.kind === "switch" ? flow.edges.filter((e) => e.from !== id || ports.includes(e.port ?? "out")) : flow.edges;
  return { nodes, edges };
}

/** A new step, somewhere it does not sit on another. */
export function add(flow: Flow, kind: string, near?: FlowNode): { flow: Flow; id: string } {
  const k = kindOf(kind);
  let n = 1;
  while (flow.nodes.some((x) => x.id === `${kind}-${n}`)) n++;
  const id = `${kind}-${n}`;
  const start = near ? { x: near.x + COLUMN, y: near.y } : { x: k.group === "Listen" ? 32 : k.group === "Decide" ? 32 + COLUMN : 32 + 2 * COLUMN, y: 32 };
  let { x, y } = start;
  while (flow.nodes.some((o) => Math.abs(o.x - x) < NODE_W && Math.abs(o.y - y) < NODE_H + 8)) y += ROW;
  return { flow: { ...flow, nodes: [...flow.nodes, { id, kind, x, y, ...k.defaults }] }, id };
}

/** Puts each step a column after whatever feeds it. */
export function arrange(flow: Flow): Flow {
  const column = new Map(flow.nodes.map((n) => [n.id, 0]));
  // A flow has no cycles, so a pass per node settles it. One drawn with a cycle is only left untidy.
  for (let i = 0; i < flow.nodes.length; i++) {
    for (const e of flow.edges) column.set(e.to, Math.max(column.get(e.to) ?? 0, (column.get(e.from) ?? 0) + 1));
  }
  const rows = new Map<number, number>();
  const nodes = flow.nodes.map((n) => {
    const c = column.get(n.id) ?? 0;
    const r = rows.get(c) ?? 0;
    rows.set(c, r + 1);
    return { ...n, x: 32 + c * COLUMN, y: 32 + r * ROW };
  });
  return { ...flow, nodes };
}

export function parseFlow(text: string | null | undefined): Flow {
  try {
    const v = JSON.parse(text ?? "") as Partial<Flow>;
    if (Array.isArray(v.nodes)) return { nodes: v.nodes, edges: Array.isArray(v.edges) ? v.edges : [] };
  } catch {
    // A draft may be anything. What cannot be drawn starts again.
  }
  return { nodes: [], edges: [] };
}

const text = (v: unknown) => (typeof v === "string" ? v : JSON.stringify(v));

const WORDS: Record<string, string> = { ">": "above", ">=": "at least", "<": "below", "<=": "at most", "==": "is", "!=": "is not" };

/** One line on a step, saying what it has been set to. */
export function summary(n: FlowNode): string {
  const who = n.device ? ` on ${text(n.device)}` : "";
  switch (n.kind) {
    case "metric":
      return `${text(n.metric) || "Say which value"}${who}`;
    case "state":
      return n.device ? text(n.device) : "Any device";
    case "event":
      return `${text(n.name) || "Say which event"}${who}`;
    case "attribute":
      return `${text(n.key) || "Say which attribute"}${who}`;
    case "timer":
    case "limit":
      return `Every ${text(n.every)}`;
    case "compare":
      return `${WORDS[text(n.op)] ?? text(n.op)} ${text(n.value)}`;
    case "range":
      return `${text(n.min)} to ${text(n.max)}`;
    case "hold":
    case "silence":
    case "delay":
      return `For ${text(n.for)}`;
    case "changed":
      return n.by === undefined ? "Any change" : `By ${text(n.by)}`;
    case "count":
      return `${text(n.of)} within ${text(n.within)}`;
    case "window":
      return `${text(n.stat)} over ${text(n.over)}`;
    case "math":
    case "combine":
      return n.formula ? text(n.formula) : "Together";
    case "all":
      return n.any ? "Any input" : "Every input";
    case "alarm":
      return `${text(n.name) || "Name it"} (${text(n.severity)})`;
    case "notify":
      return text(n.title) || "Say what";
    case "command":
      return text(n.operation) || "Say which";
    case "export":
      return `To ${text(n.to) || "which?"}`;
    case "record_event":
    case "record_metric":
      return text(n.name) || "Name it";
    case "record_attribute":
      return text(n.key) || "Name it";
    case "field":
      return text(n.path) || "Say where";
    case "switch":
      return ((n.cases as string[]) ?? []).join(", ");
    default:
      return kindOf(n.kind).says;
  }
}

export interface Effect {
  effect: string;
  [key: string]: unknown;
}

/** What a simulated flow did, as a sentence. */
export function sayEffect(e: Effect): string {
  const on = e.device ? ` on ${text(e.device)}` : "";
  switch (e.effect) {
    case "alarm":
      return e.raised ? `Raises “${text(e.name)}”${on}` : `Clears “${text(e.name)}”${on}`;
    case "event":
      return `Records ${text(e.name)}${on}`;
    case "metric":
      return `Records ${text(e.name)} = ${text(e.value)}${on}`;
    case "attribute":
      return `Sets ${text(e.key)} to ${text(e.value)}${on}`;
    case "notify":
      return `Tells people: ${text(e.title)}`;
    case "export":
      return `Sends to ${text(e.to)}`;
    case "command":
      return `Asks for ${text(e.operation)}${on}. A person or an envelope decides.`;
    default:
      return e.effect;
  }
}

export interface ScriptLine {
  at: number;
  device: string;
  what: "metric" | "state" | "event" | "attribute";
  name: string;
  value: string;
}

/** A first script from the flow's own sources, so that simulating starts from something real. */
export function firstScript(flow: Flow): ScriptLine[] {
  const lines: ScriptLine[] = [];
  flow.nodes.forEach((n) => {
    const device = text(n.device ?? "DEVICE-1").replace("*", "1");
    if (n.kind === "metric") lines.push({ at: 0, device, what: "metric", name: text(n.metric), value: "1" });
    if (n.kind === "state") lines.push({ at: 0, device, what: "state", name: "", value: "Running" });
    if (n.kind === "event") lines.push({ at: 0, device, what: "event", name: text(n.name), value: "true" });
    if (n.kind === "attribute") lines.push({ at: 0, device, what: "attribute", name: text(n.key), value: "1" });
  });
  return lines;
}

const reading = (v: string): unknown => {
  try {
    return JSON.parse(v);
  } catch {
    return v;
  }
};

/** A script as the server reads it. */
export function scriptBody(lines: ScriptLine[]) {
  return lines.map((l) => (l.what === "state" ? { at: l.at, device: l.device, state: l.value } : { at: l.at, device: l.device, [l.what]: l.name, value: reading(l.value) }));
}

/** What to keep the clock running for after the script: long enough to see a hold or a silence fire. */
export function waitFor(flow: Flow, lines: ScriptLine[]): number {
  const secs = (v: unknown) => {
    const m = /^(\d+(?:\.\d+)?)(ms|s|m|h)?$/.exec(text(v));
    if (!m) return 0;
    return Number(m[1]) * { ms: 0.001, s: 1, m: 60, h: 3600 }[m[2] ?? "s"]!;
  };
  const longest = flow.nodes.reduce((most, n) => Math.max(most, ["hold", "silence", "delay"].includes(n.kind) ? secs(n.for) : 0, n.kind === "timer" ? secs(n.every) : 0), 0);
  const last = lines.reduce((most, l) => Math.max(most, l.at), 0);
  return Math.min(6 * 3600, Math.ceil(last + longest + 1));
}

/** Ways to begin, for someone looking at an empty canvas. */
export const STARTS: { name: string; says: string; flow: Flow }[] = [
  {
    name: "Alarm when a number stays high",
    says: "A number above a limit for a while raises an alarm, and coming back down clears it.",
    flow: {
      nodes: [
        { id: "reading", kind: "metric", metric: "temp.c", x: 32, y: 64 },
        { id: "high", kind: "compare", op: ">", value: 80, x: 276, y: 64 },
        { id: "lasted", kind: "hold", for: "30s", x: 520, y: 32 },
        { id: "alarm", kind: "alarm", name: "Running hot", severity: "Warning", x: 764, y: 64 },
      ],
      edges: [
        { from: "reading", to: "high" },
        { from: "high", port: "yes", to: "lasted" },
        { from: "high", port: "no", to: "lasted", input: "reset" },
        { from: "high", port: "no", to: "alarm", input: "clear" },
        { from: "lasted", to: "alarm" },
      ],
    },
  },
  {
    name: "Tell someone when a device goes quiet",
    says: "Nothing heard for a while tells the people who look after it.",
    flow: {
      nodes: [
        { id: "reading", kind: "metric", metric: "temp.c", x: 32, y: 64 },
        { id: "quiet", kind: "silence", for: "5m", x: 276, y: 64 },
        { id: "tell", kind: "notify", title: "{device} has gone quiet", x: 520, y: 64 },
      ],
      edges: [
        { from: "reading", to: "quiet" },
        { from: "quiet", to: "tell" },
      ],
    },
  },
  {
    name: "Ask for a charge when the battery is low",
    says: "A low battery asks for a command. It waits for a person unless an envelope allows it.",
    flow: {
      nodes: [
        { id: "battery", kind: "metric", metric: "battery.percent", x: 32, y: 64 },
        { id: "low", kind: "compare", op: "<", value: 20, x: 276, y: 64 },
        { id: "ask", kind: "command", operation: "robot.charge", every: "10m", x: 520, y: 64 },
      ],
      edges: [
        { from: "battery", to: "low" },
        { from: "low", port: "yes", to: "ask" },
      ],
    },
  },
  {
    name: "Work out power from volts and amps",
    says: "Two numbers from one device become a third, kept like any other.",
    flow: {
      nodes: [
        { id: "volts", kind: "metric", metric: "volts", x: 32, y: 32 },
        { id: "amps", kind: "metric", metric: "amps", x: 32, y: 128 },
        { id: "power", kind: "combine", formula: "[v] * [a] / 1000", fresh: "30s", x: 276, y: 80 },
        { id: "keep", kind: "record_metric", name: "power.kw", unit: "kW", x: 520, y: 80 },
      ],
      edges: [
        { from: "volts", to: "power", input: "v" },
        { from: "amps", to: "power", input: "a" },
        { from: "power", to: "keep" },
      ],
    },
  },
];
