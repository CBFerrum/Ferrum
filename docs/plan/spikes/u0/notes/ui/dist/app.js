// Notes (spike U0): handles the "pick note" intent in a dialog other apps open, and is the page
// of a companion window (a pretend call) that outlives the pages of other apps.
const shell = globalThis.ferrum?.shell;
const boot = shell?.bootstrap;
const main = document.getElementById("main");
const base = boot?.app?.routes?.notes ?? "/notes/";
const NOTES = [
  { id: "n1", title: "Agenda for the supplier review" },
  { id: "n2", title: "Questions for the bank" },
  { id: "n3", title: "Launch checklist" },
];
const page = location.pathname.slice(base.length).split("/").filter(Boolean)[0] ?? "";

const el = (tag, text) => {
  const e = document.createElement(tag);
  if (text) e.textContent = text;
  return e;
};

async function pick() {
  shell?.title("Pick a note");
  const asked = await shell.intent.request();
  const h = el("h2", "Pick a note");
  const why = el("p", `Asked by ${asked.from ?? "an app"} for ${JSON.stringify(asked.data)}.`);
  const list = el("ul");
  list.className = "notes";
  for (const n of NOTES) {
    const li = el("li");
    const b = el("button", n.title);
    b.type = "button";
    b.addEventListener("click", () => shell.intent.done({ id: n.id, title: n.title }));
    li.append(b);
    list.append(li);
  }
  const cancel = el("button", "Cancel");
  cancel.type = "button";
  cancel.addEventListener("click", () => shell.intent.cancel());
  const card = el("section");
  card.className = "card";
  card.append(h, why, list, cancel);
  main.append(card);
}

function call() {
  shell?.title("Call");
  const started = Date.now();
  const card = el("section");
  card.className = "card";
  const t = el("p");
  t.id = "elapsed";
  card.append(el("h2", "In a call"), el("p", "This window stays open while you work in other apps."), t);
  main.append(card);
  setInterval(() => {
    t.textContent = `${Math.round((Date.now() - started) / 1000)} s`;
  }, 500);
}

function list() {
  shell?.title("Notes");
  const card = el("section");
  card.className = "card";
  card.append(el("h2", "Notes"));
  for (const n of NOTES) card.append(el("p", n.title));
  const back = el("button", "Back to Deals");
  back.type = "button";
  back.addEventListener("click", () => shell.navigate("/deals/"));
  card.append(back);
  main.append(card);
}

if (page === "pick") pick();
else if (page === "call") call();
else list();
shell?.ready();
