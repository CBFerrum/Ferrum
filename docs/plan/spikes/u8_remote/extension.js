// An extension's code (a rating widget for a helpdesk ticket), in a sandboxed frame. It describes
// its UI as a tree of the host's components; functions become handles the host calls back.

let port = null;
const fns = new Map();
let next = 1;
const fn = (f) => {
  const id = next++;
  fns.set(id, f);
  return { $fn: id };
};
const h = (type, props = {}, ...children) => ({ type, props, children: children.flat().filter(Boolean) });

let rating = 0;
let comment = "";
let sent = false;

function render() {
  fns.clear();
  const stars = [1, 2, 3, 4, 5].map((n) =>
    h("Button", { label: n <= rating ? "★" : "☆", label_aria: `${n} of 5`, onPress: fn(() => ((rating = n), render())) }),
  );
  const tree = h(
    "Stack",
    {},
    h("Heading", { text: "How did we do?" }),
    h("Row", {}, ...stars, rating ? h("Badge", { text: `${rating} of 5`, tone: rating >= 4 ? "green" : rating <= 2 ? "red" : "" }) : null),
    h("TextField", { label: "Anything to add?", value: comment, onChange: fn((v) => ((comment = String(v)), render())) }),
    h("Row", {}, h("Button", { label: sent ? "Thank you" : "Send", kind: "primary", disabled: !rating || sent, onPress: fn(() => ((sent = true), render())) })),
    // What the host must refuse: markup it does not know, and links that are not its own pages.
    h("Script", { text: "alert(1)" }),
    h("Link", { text: "Elsewhere", href: "javascript:alert(document.cookie)" }),
    h("Link", { text: "The ticket's page", href: "/helpdesk/t/42" }),
    // ?big=N: N more rows, to time larger trees.
    ...Array.from({ length: Math.min(Number(new URLSearchParams(location.search).get("big")) || 0, 480) }, (_, i) => h("Text", { text: `Row ${i + 1}`, tone: "muted" })),
  );
  port.postMessage({ type: "render", tree });
}

window.addEventListener("message", (e) => {
  if (e.source !== window.parent || e.data?.type !== "init" || !e.ports[0]) return;
  port = e.ports[0];
  port.onmessage = ({ data }) => {
    if (data?.type === "event") fns.get(data.fn)?.(data.value);
  };
  // What this code can reach of the host: nothing.
  let probe = [];
  try {
    probe.push(`parent document: ${window.parent.document ? "reachable" : "unreachable"}`);
  } catch {
    probe.push("parent document: unreachable");
  }
  try {
    probe.push(`cookie: ${JSON.stringify(document.cookie)}`);
  } catch {
    probe.push("cookie: unreadable");
  }
  try {
    localStorage.getItem("x");
    probe.push("storage: reachable");
  } catch {
    probe.push("storage: unreachable");
  }
  probe.push(`origin: ${self.origin}`);
  port.postMessage({ type: "probe", result: probe.join("; ") });
  render();
});
