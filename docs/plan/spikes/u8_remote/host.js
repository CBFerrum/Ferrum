// The host side of remote rendering (docs/plan/spike-u8-remote-rendering.md): an extension's code
// runs in a sandboxed frame (an opaque origin, no DOM of ours, no session) and sends a tree of
// components; this page draws only the components it knows, with props it checks, and sends events
// back. Nothing the extension sends becomes markup.

const MAX_NODES = 500;
const MAX_DEPTH = 20;
const log = (line) => (document.querySelector("#log").textContent += `${line}\n`);
const slot = document.querySelector("#slot");

/** The components an extension may use, each drawn from checked props. */
const COMPONENTS = {
  Stack: () => el("div", "r-stack"),
  Row: () => el("div", "r-row"),
  Heading: (p) => text(el("h2", "r-heading"), p.text),
  Text: (p) => text(el("p", p.tone === "muted" ? "muted" : ""), p.text),
  Badge: (p) => text(el("span", `r-badge ${["green", "red"].includes(p.tone) ? p.tone : ""}`), p.text),
  Button: (p, fire) => {
    const b = text(el("button", `r-button ${p.kind === "primary" ? "primary" : ""}`), p.label);
    b.type = "button";
    b.disabled = p.disabled === true;
    if (p.label_aria) b.setAttribute("aria-label", String(p.label_aria).slice(0, 100));
    b.addEventListener("click", () => fire(p.onPress));
    return b;
  },
  TextField: (p, fire) => {
    const wrap = el("label", "r-field");
    wrap.append(String(p.label ?? "").slice(0, 80));
    const input = document.createElement("input");
    input.value = String(p.value ?? "").slice(0, 2000);
    input.maxLength = 2000;
    input.addEventListener("change", () => fire(p.onChange, input.value));
    wrap.append(input);
    return wrap;
  },
  Link: (p) => {
    const a = text(el("a"), p.text);
    // Only this site's own pages: no javascript:, no other sites.
    const href = String(p.href ?? "");
    if (href.startsWith("/") && !href.startsWith("//") && !href.includes("\\")) a.href = href;
    else log(`refused a link to ${JSON.stringify(href).slice(0, 60)}`);
    return a;
  },
};

function el(tag, className = "") {
  const e = document.createElement(tag);
  if (className.trim()) e.className = className.trim();
  return e;
}

function text(e, t) {
  e.textContent = String(t ?? "").slice(0, 2000);
  return e;
}

/** Draws a tree; unknown components and too-large trees are refused, not guessed. */
function draw(tree, fire) {
  let nodes = 0;
  const walk = (node, depth) => {
    if (!node || typeof node !== "object") return null;
    if (++nodes > MAX_NODES || depth > MAX_DEPTH) throw new Error("the tree is too large");
    const make = COMPONENTS[node.type];
    if (!make) {
      log(`refused an unknown component ${JSON.stringify(String(node.type)).slice(0, 40)}`);
      return null;
    }
    const e = make(node.props ?? {}, fire);
    for (const child of Array.isArray(node.children) ? node.children : []) {
      const c = walk(child, depth + 1);
      if (c) e.append(c);
    }
    return e;
  };
  return walk(tree, 0);
}

// The extension's frame: sandboxed, no allow-same-origin, its own file only.
const frame = document.createElement("iframe");
frame.sandbox = "allow-scripts";
frame.src = `./extension.html${location.search}`;
frame.hidden = true;
document.body.append(frame);

const channel = new MessageChannel();
const port = channel.port1;
let renders = 0;
port.onmessage = ({ data }) => {
  if (data?.type === "render") {
    const started = performance.now();
    const fire = (handler, value) => {
      if (typeof handler?.$fn === "number") port.postMessage({ type: "event", fn: handler.$fn, value });
    };
    try {
      const drawn = draw(data.tree, fire);
      slot.replaceChildren(...(drawn ? [drawn] : []));
    } catch (e) {
      log(`refused a render: ${e.message}`);
    }
    renders += 1;
    document.body.dataset.renders = String(renders);
    log(`render ${renders} drawn in ${(performance.now() - started).toFixed(1)} ms`);
  } else if (data?.type === "probe") {
    log(`probe: ${data.result}`);
  }
};
frame.addEventListener("load", () => frame.contentWindow.postMessage({ type: "init" }, "*", [channel.port2]));
