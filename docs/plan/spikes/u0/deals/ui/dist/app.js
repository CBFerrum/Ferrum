// Deals (spike U0): an app with its own interface, using the shell API from the app's side.
const shell = globalThis.ferrum?.shell;
const boot = shell?.bootstrap;
const main = document.getElementById("main");
const base = boot?.app?.routes?.deals ?? "/deals/";
let dirty = false;
const log = [];

function note(line) {
  log.unshift(`${new Date().toLocaleTimeString()} ${line}`);
  render();
}

function route() {
  const rest = location.pathname.startsWith(base) ? location.pathname.slice(base.length) : "";
  return rest.split("/").filter(Boolean);
}

function go(path) {
  history.pushState(null, "", base + path);
  render();
}

function render() {
  const [first, second] = route();
  const deal = first && first !== "new" ? first : null;
  shell?.title(deal ? `Deal ${deal}` : first === "new" ? "New deal" : "Deals");
  shell?.breadcrumbs(deal ? ["Deals", `Deal ${deal}`] : []);
  main.innerHTML = "";
  const card = (title, ...children) => {
    const c = document.createElement("section");
    c.className = "card";
    const h = document.createElement("h2");
    h.textContent = title;
    c.append(h, ...children);
    main.append(c);
  };
  const button = (label, onclick, primary = false) => {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = label;
    if (primary) b.className = "primary";
    b.addEventListener("click", onclick);
    return b;
  };
  const p = (text) => {
    const e = document.createElement("p");
    e.textContent = text;
    return e;
  };

  card(
    deal ? `Deal ${deal}` : "Deals",
    p(`Signed in as ${boot?.person?.name} in ${boot?.workspace?.name}; release ${boot?.app?.release?.slice(0, 12)}; route ${location.pathname}${second ? ` (${second})` : ""}.`),
    button("Deal 42", () => go("42")),
    button("All deals", () => go("")),
    button("Pick a note", async () => {
      try {
        const out = await shell.intent({ action: "pick", type: "note", data: { for: deal ?? "deals" } });
        note(out.status === "done" ? `picked ${JSON.stringify(out.result)} from ${out.app}` : "picking cancelled");
      } catch (e) {
        note(`intent failed: ${e.code ?? e.message}`);
      }
    }, true),
    button("Open Notes", () => shell.navigate("/notes/")),
    button("Start a call", () => shell.companion({ kind: "call", url: "/notes/call" })),
  );

  const check = document.createElement("input");
  check.type = "checkbox";
  check.checked = dirty;
  check.id = "dirty";
  check.addEventListener("change", () => {
    dirty = check.checked;
  });
  const label = document.createElement("label");
  label.htmlFor = "dirty";
  label.textContent = " Unsaved changes (asks before leaving)";
  card("Leaving", check, label);

  const pre = document.createElement("pre");
  pre.textContent = log.join("\n") || "Nothing yet.";
  pre.id = "log";
  card("What happened", pre);
}

window.addEventListener("popstate", render);
shell?.leaveGuard(() => dirty);
render();
shell?.live.subscribe(["Customer"], (change) => note(`live: ${change.event} ${change.entity} ${change.id}`));
shell?.on("outdated", () => note("a newer release is available"));
shell?.ready();
window.__deals = { note, log };
