#!/usr/bin/env python3
"""One path for people and agents (docs/registry/agent-parity.md, docs/app-contract.md, docs/guides/06).

    scripts/agent_parity.py               # CI runs this: fails on a finding the registry lacks, a stale row, a row
                                          # without a kind, or more debt than allowed
    scripts/agent_parity.py --write       # rewrites both tables from the code, keeps the kinds given, writes
                                          # `unclassified` for a new route, lowers the allowed numbers, never raises them
    scripts/agent_parity.py --app <dir>   # one app checkout (scripts/app_ci.sh): its findings against this registry

Every change an app makes to data on a person's request goes through a declared operation. Two things are found:

* a route that writes: each write route of the platform's controllers (all listed, each with a kind a person gives),
  and each arm of an app's `Site` that reaches a write (the kernel's record calls, the outbox, SQL that inserts,
  updates or deletes) through the functions it calls, whether or not it also calls the gate (`invoke`, `invoke_tx`,
  `call_tx`): a route that runs the operation and then writes something of its own does what an agent would not;
* an action a person can press (not `hidden`) whose `contract:` lacks `effect`, `returns` or `completion`.

Reads `cargo metadata` as scripts/mutation_paths.py does (the apps the lock compiles in, and this workspace's own),
their sources but not their tests.
"""

import re
import sys
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mutation_paths  # noqa: E402
import secret_paths  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
REGISTRY = ROOT / "docs/registry/agent-parity.md"
UNCLASSIFIED = "unclassified"
WRITES = ("POST", "PUT", "PATCH", "DELETE")

WRITE = re.compile(
    r"\b(k|kernel|self\.k|self\.kernel|hc\.kernel|st\.kernel)\s*\.\s*"
    r"(insert|insert_tx|insert_submit|insert_idempotent_with|update|update_tx|set_values|set_values_tx|"
    r"delete|delete_tx|submit|submit_tx|cancel|cancel_tx|amend|run_action|run_action_tx|"
    r"run_action_idempotent|transition|transition_tx|merge_records_with|add_comment|assign|set_tags|"
    r"store_file|remove_file|issue_link|bulk_job)\s*\(|\boutbox::enqueue\b"
)
SQL = re.compile(r'\b(insert\s+into|update\s+[a-z_."{}]+\s+set|delete\s+from)\b', re.I)
# A match arm on (method, path) or (anything, method, path): the methods are literals, `|`-joined, maybe bound.
ARM = re.compile(
    r'\(\s*(?P<pre>[^(),"\n]*,\s*)?(?P<m>(?:[a-z_]+\s*@\s*)?\(?\s*"[A-Z]+"(?:\s*\|\s*"[A-Z]+")*\s*\)?)\s*,\s*'
    r"(?P<pat>[^\n]*?)\)\s*(?:if\b[^\n]*?)?=>"
)
# Any other test of a write method: `== "POST"`, `"PUT" =>`, `"PUT" |`, `["POST", ...].contains`.
METHOD_TEST = re.compile(r'==\s*"(?:POST|PUT|PATCH|DELETE)"|"(?:POST|PUT|PATCH|DELETE)"(?:\s*\|\s*"[A-Z]+")*\s*(?:=>|\|)|\[\s*"[A-Z]+"[^\]\n]*\]\s*\.\s*contains')
CALL = re.compile(r"\b([a-z_][a-z0-9_]*(?:\s*::\s*[a-z_][a-z0-9_]*)*)\s*\(")
FN = re.compile(r"\bfn\s+([a-z_][a-z0-9_]*)")
SITE = re.compile(r"\bimpl\s*(?:<[^>]*>\s*)?(?:[\w:]+::)?Site\s+for\s+\w+")
BODY = re.compile(r"[{;]")
RAW = re.compile(r'b?r(#*)"')
CHAR = re.compile(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^\\'\n])'")
WRAPPERS = {"ok", "json", "return"}
KIND = re.compile(r"^(operation: (?P<op>\S+)|identity|protocol: \S.*|public|portal|administration|collaboration|debt)$")
# `*.create`, `*.update`, `*.delete`, `*.submit`, `*.cancel`, `*.<action>` or `*.transition.<action>`.
CAPABILITY = re.compile(r"^\*\.(transition\.)?(\{action\}|[a-z_]+)$")
ROUTE_ROW = re.compile(r"^\| `(?P<where>[^`]+)` \| (?P<method>[A-Z/]+) \| `(?P<route>.+?)` \| `(?P<handler>[^`]+)` \| (?P<kind>.+?) \|$")
ACTION_ROW = re.compile(r"^\| ([a-z0-9_-]+) \| ([a-z0-9_]+) \| (\w+) \| (\w+) \|$")


# ─── reading Rust ───

def views(text: str) -> tuple:
    """The code a check reads: (`code` with comments blanked, `bare` with string and character literals blanked too),
    both the length of `text` so that offsets agree, and everything from `#[cfg(test)]` on left out."""
    kept = dict(secret_paths.code_lines(text))
    text = "\n".join(kept.get(n, "") for n in range(1, len(text.split("\n")) + 1))
    code, bare = list(text), list(text)
    i, n = 0, len(text)

    def blank(a, b, both):
        for j in range(a, b):
            if text[j] != "\n":
                bare[j] = " "
                if both:
                    code[j] = " "

    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            blank(i, j, True)
            i = j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                depth += 1 if text.startswith("/*", j) else -1 if text.startswith("*/", j) else 0
                j += 2 if text.startswith(("/*", "*/"), j) else 1
            blank(i, j, True)
            i = j
        elif c in "br" and not (text[i - 1 : i].isalnum() or text[i - 1 : i] == "_") and (m := RAW.match(text, i)):
            end = text.find('"' + m.group(1), m.end())
            end = n if end < 0 else end + 1 + len(m.group(1))
            blank(m.end(), end - 1 - len(m.group(1)), False)
            i = end
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            blank(i + 1, j, False)
            i = j + 1
        elif c == "'" and (m := CHAR.match(text, i)):
            blank(i + 1, m.end() - 1, False)
            i = m.end()
        else:
            i += 1
    return "".join(code), "".join(bare)


def closing(bare: str, at: int) -> int:
    """The offset just past the bracket that closes the one at `at`."""
    pairs, depth = {"(": ")", "[": "]", "{": "}"}, 0
    for j in range(at, len(bare)):
        if bare[j] in pairs:
            depth += 1
        elif bare[j] in ")]}":
            depth -= 1
            if depth == 0:
                return j + 1
    return len(bare)


def arm_end(bare: str, at: int) -> int:
    """Where a match arm's right-hand side that starts at `at` ends: its block, or its expression up to the comma."""
    j = at
    while j < len(bare) and bare[j].isspace():
        j += 1
    if j < len(bare) and bare[j] == "{":
        return closing(bare, j)
    depth = 0
    while j < len(bare):
        if bare[j] in "([{":
            depth += 1
        elif bare[j] in ")]}":
            if depth == 0:
                return j
            depth -= 1
        elif bare[j] == "," and depth == 0:
            return j
        j += 1
    return j


class App:
    """An app's sources: each function's body, by name, and what each body reaches."""

    def __init__(self, root: Path):
        self.files = {}  # relative path -> (code, bare)
        self.fns = {}  # name -> [(file, start, end)]
        self.methods = {}  # (file, start, end) -> whether it takes `self`
        for path in secret_paths.sources(root):
            if path.suffix != ".rs":
                continue
            rel = path.relative_to(root).as_posix()
            code, bare = views(path.read_text(errors="replace"))
            self.files[rel] = (code, bare)
            for m in FN.finditer(bare):
                body = BODY.search(bare, m.end())
                while body and bare[body.start()] == "{" and bare.count("(", m.end(), body.start()) != bare.count(")", m.end(), body.start()):
                    body = BODY.search(bare, body.end())
                if body and bare[body.start()] == "{":
                    f = (rel, body.start(), closing(bare, body.start()))
                    self.fns.setdefault(m.group(1), []).append(f)
                    self.methods[f] = bool(re.match(r"[^(]*\(\s*(&\s*)?(mut\s+)?self\b", bare[m.end():body.start()]))
        self.direct, self.called = {}, {}

    def calls(self, rel: str, a: int, b: int) -> set:
        """The bodies the code between `a` and `b` of a file calls, resolved the way a reader would: in its own file
        first, then in the module a path names, then anywhere in the app. `x.f()` calls a method, `f()` a function."""
        if (rel, a, b) in self.called:
            return self.called[(rel, a, b)]
        out = self.called[(rel, a, b)] = set()
        bare = self.files[rel][1]
        for m in CALL.finditer(bare, a, b):
            path = [p.strip() for p in m.group(1).split("::")]
            method = bare[max(0, m.start() - 80) : m.start()].rstrip().endswith(".")
            found = [f for f in self.fns.get(path[-1], []) if self.methods[f] == method]
            module = [p for p in path[:-1] if p not in ("crate", "self", "super")]
            if module:
                found = [f for f in found if Path(f[0]).stem == module[-1] or Path(f[0]).parent.name == module[-1]]
            elif any(f[0] == rel for f in found):
                found = [f for f in found if f[0] == rel]
            out |= set(found)
        return out

    def writes(self, rel: str, a: int, b: int) -> bool:
        """Whether the code between `a` and `b`, or anything it calls (eight calls deep), writes. Calling the gate is
        not a write; a write beside it still is."""
        seen, frontier = set(), self.calls(rel, a, b)
        for _ in range(8):
            frontier -= seen
            seen |= frontier
            frontier = set().union(*(self.calls(*f) for f in frontier)) if frontier else set()
        return any(self.writes_here(*f) for f in [(rel, a, b), *seen])

    def writes_here(self, rel: str, a: int, b: int) -> bool:
        if (rel, a, b) not in self.direct:
            code, bare = self.files[rel]
            sql = any(SQL.search(s.group(0)) for s in secret_paths.STRING.finditer(code, a, b))
            self.direct[(rel, a, b)] = sql or bool(WRITE.search(bare, a, b))
        return self.direct[(rel, a, b)]

    def handlers(self) -> list:
        """The `handle` body of each `impl Site for`, and every body it reaches."""
        out = []
        for rel, (_, bare) in self.files.items():
            for m in SITE.finditer(bare):
                block = bare.index("{", m.end())
                end = closing(bare, block)
                for f in self.fns.get("handle", []):
                    if f[0] == rel and block < f[1] < end:
                        out.append(f)
        seen, frontier = set(), set(out)
        while frontier:
            seen |= frontier
            frontier = set().union(*(self.calls(*f) for f in frontier)) - seen
        return sorted(seen)


def site_routes(app: str, root: Path) -> list:
    """(where, methods, route, handler) for each route of an app's `Site` that writes."""
    a = App(root)
    found = []
    for rel, start, end in a.handlers():
        code, bare = a.files[rel]
        arms = []
        for m in ARM.finditer(code, start, end):
            methods = [x for x in re.findall(r'"([A-Z]+)"', m.group("m")) if x in WRITES]
            rhs = (m.end(), arm_end(bare, m.end()))
            arms.append((m.start("m"), rhs[1]))
            if methods and a.writes(rel, *rhs):
                route = re.sub(r"\s+", " ", ((m.group("pre") or "") + m.group("pat")).strip())
                found.append((f"{app}:{rel}", "/".join(methods), route, handler(a, bare[rhs[0]:rhs[1]])))
        for m in METHOD_TEST.finditer(code, start, end):
            if any(x <= m.start() < y for x, y in arms):
                continue
            line_end = code.find("\n", m.end())
            arrow = code.find("=>", m.start(), line_end)
            span = (arrow + 2, arm_end(bare, arrow + 2)) if arrow >= 0 else (start, end)
            if a.writes(rel, *span):
                line = code[code.rfind("\n", 0, m.start()) + 1:line_end]
                methods = [x for x in re.findall(r'"([A-Z]+)"', m.group(0)) if x in WRITES] or ["?"]
                found.append((f"{app}:{rel}", "/".join(methods), re.sub(r"\s+", " ", line.strip()), "?"))
    return sorted(found)


def handler(a: App, rhs: str) -> str:
    """What an arm calls to do its work: the first of the app's functions it hands the kernel, or else the first of
    the app's functions, or else the first call."""
    calls = [(re.sub(r"\s+", "", m.group(1)), re.match(r"\s*&?\s*(k|kernel)\b", rhs[m.end():])) for m in CALL.finditer(rhs)]
    calls = [(n, kernel) for n, kernel in calls if n.split("::")[-1] not in WRAPPERS]
    mine = [(n, kernel) for n, kernel in calls if n.split("::")[-1] in a.fns]
    return next((n for n, kernel in mine if kernel), mine[0][0] if mine else calls[0][0] if calls else "?")


# ─── reading packages ───

def load(path: Path):
    import yaml

    # serde_yaml takes `Why are you cancelling?, type: text` in a flow mapping; PyYAML stops at the `?`. Only names,
    # `hidden` and the contract's keys are read here, so the question mark is left out.
    return yaml.load(re.sub(r"\?(?=[,}\]])", "", path.read_text()), Loader=getattr(yaml, "CSafeLoader", yaml.SafeLoader))


def actions(app: str, root: Path) -> list:
    """(app, package, entity, action) for each action a person can press that has no full contract."""
    out = []
    for manifest in sorted(root.rglob("package.yaml")):
        parts = manifest.relative_to(root).parts
        if {"tests", "target", "node_modules", ".git"} & set(parts):
            continue
        pkg = load(manifest) or {}
        listed = []
        for f in sorted((manifest.parent / "entities").glob("*.yaml")):
            doc = load(f) or []
            for e in doc if isinstance(doc, list) else [doc]:
                listed += [(e.get("name"), a) for a in e.get("actions") or []]
        for o in pkg.get("overlays") or []:
            listed += [(o.get("entity"), op.get("action") or {}) for op in o.get("ops") or [] if op.get("op") == "add_action"]
        for entity, a in listed:
            contract = a.get("contract") or {}
            if not a.get("hidden") and not all(contract.get(k) for k in ("effect", "returns", "completion")):
                out.append((app, pkg.get("package"), entity, a.get("name")))
    return out


# ─── the platform's routes ───

def platform_routes(root: Path = ROOT) -> list:
    """(where, method, route, handler) for each write route the platform's controllers add: all listed."""
    out = []
    for path in sorted((root / "server/src/controllers").glob("*.rs")):
        code, bare = views(path.read_text(errors="replace"))
        alias = {b: a for a, b in re.findall(r"\b(get|post|put|patch|delete|any)\s+as\s+(\w+)", code)}
        rel = path.relative_to(root).as_posix()
        prefix = ""
        for m in re.finditer(r'Routes::new\(\)|\.prefix\(\s*"([^"]*)"\s*\)|\.add\(\s*"([^"]*)"\s*,', code):
            if m.group(0).startswith("Routes"):
                prefix = ""
            elif m.group(1) is not None:
                prefix = m.group(1)
            else:
                chain = code[m.end():closing(bare, m.start() + 4) - 1]
                for verb, fn in re.findall(r"(?<![\w])(\w+)\s*\(\s*([a-z_][a-z0-9_]*)\s*\)", chain):
                    verb = alias.get(verb, verb)
                    if verb in ("post", "put", "patch", "delete", "any"):
                        out.append((rel, verb.upper(), prefix + m.group(2), fn))
    return out


def tools(root: Path = ROOT) -> set:
    """The MCP tools' names, which an `operation:` kind may name."""
    names = set()
    for p in [*sorted((root / "crates/mcp/src").glob("*.rs")), *sorted((root / "server/src").glob("*_mcp.rs"))]:
        t = p.read_text(errors="replace")
        names |= set(re.findall(r'"name":\s*"([a-z_]+)"', t)) | set(re.findall(r'\btool(?:_definition)?\(\s*"([a-z_]+)"', t))
    return names


def apps() -> dict:
    """Each app the lock compiles in, and this workspace's own, by short name: its checkout's root."""
    out = dict(mutation_paths.repositories())
    for name in secret_paths.WORKSPACE_APPS:
        short = name.removeprefix("ferrum-")
        out[short] = ROOT / "crates" / short
    return out


# ─── the registry ───

def read_registry() -> dict:
    reg = {"allowed_routes": None, "allowed_actions": None, "routes": [], "actions": []}
    if not REGISTRY.exists():
        return reg
    for line in REGISTRY.read_text().split("\n"):
        if m := re.match(r"^Routes in debt allowed: (\d+)", line):
            reg["allowed_routes"] = int(m.group(1))
        elif m := re.match(r"^Actions without a contract allowed: (\d+)", line):
            reg["allowed_actions"] = int(m.group(1))
        elif m := ROUTE_ROW.match(line):
            reg["routes"].append((m["where"], m["method"], m["route"].replace("\\|", "|"), m["handler"], m["kind"]))
        elif m := ACTION_ROW.match(line):
            reg["actions"].append(m.groups())
    return reg


HEAD = """# Agent parity

Every change an app makes to data on a person's request goes through a declared operation: a record operation or an
action with a contract, run through the kernel's record pipeline or the gate, so that an agent can do what a screen does
by the same operation (docs/app-contract.md, docs/guides/06). `scripts/agent_parity.py` keeps this file true: CI fails
on a write route or an action the tables lack, a row the code no longer has, a row without a kind, or more debt than the
numbers below allow. `--write` rewrites the tables from the code, keeps each kind given, writes `unclassified` for a new
route (which fails until a person gives it a kind), and lowers the two numbers to today's counts; it never raises them.

A route is listed when it writes: every write route of the platform's controllers, and every arm of an app's `Site` that
reaches a write (the kernel's record calls, the outbox, SQL that inserts, updates or deletes), whether or not it also
calls the gate: a route that runs an operation and then writes something of its own does what an agent would not. Hooks,
event sinks, upkeep and jobs are not routes: an agent triggers them by the same operation a person does.

| Kind | Meaning | Debt |
|---|---|---|
| `operation: <capability or tool>` | the same kernel call as a catalog operation (`*.create`, `*.update`, `*.delete`, `*.submit`, `*.cancel`, `*.<action>`, `*.transition.<action>`) or a named MCP tool | no |
| `identity` | sign-in, sessions, passkeys, OAuth, SAML, SCIM | no |
| `protocol: <name>` | a protocol an outside program speaks: DAV, WOPI, an inbound provider webhook, `/mcp` itself | no |
| `public` | anonymous visitors on `/api/site/…` or signed links | no |
| `portal` | outside people signed in to the portal acting on their own records | no |
| `administration` | platform configuration (settings, users, roles, vault, apps); agents reach it under their grants | no |
| `collaboration` | comments, assignments, tags, follows on any record | no |
| `debt` | must become an operation; the allowed number only falls | yes |

The tables shrink when a route's writes move into an action with a contract that the route invokes, and when an action
gets a contract with `effect`, `returns` and `completion` (the recipe in docs/guides/06). The next `--write` lowers the
numbers.

"""


def esc(s: str) -> str:
    return s.replace("|", "\\|")


def write_registry(routes: list, acts: list, reg: dict) -> None:
    kinds = {}
    for r in reg["routes"]:
        kinds.setdefault(r[:4], []).append(r[4])
    rows = [(*r, (kinds.get(r) or [UNCLASSIFIED]).pop(0)) for r in sorted(routes)]
    debt = sum(r[4] == "debt" for r in rows)
    # A number not set yet is set from the code, the routes' once every route has a kind; after that it only falls.
    allowed_routes = reg["allowed_routes"]
    if allowed_routes is not None:
        allowed_routes = min(allowed_routes, debt)
    elif all(r[4] != UNCLASSIFIED for r in rows):
        allowed_routes = debt
    allowed_actions = len(acts) if reg["allowed_actions"] is None else min(reg["allowed_actions"], len(acts))
    unset = "(set by the first --write after every route has a kind)"
    head = HEAD
    if REGISTRY.exists():
        head = re.split(r"^(?:Routes in debt allowed:|Actions without a contract allowed:|## Routes)", REGISTRY.read_text(), maxsplit=1, flags=re.M)[0]
    text = head + f"Routes in debt allowed: {unset if allowed_routes is None else allowed_routes}\nActions without a contract allowed: {allowed_actions}\n\n## Routes\n\n"
    text += "| Where | Method | Route | Handler | Kind |\n|---|---|---|---|---|\n"
    text += "".join(f"| `{w}` | {m} | `{esc(p)}` | `{h}` | {k} |\n" for w, m, p, h, k in rows)
    text += "\n## Actions without a contract\n\n| App | Package | Entity | Action |\n|---|---|---|---|\n"
    text += "".join(f"| {a} | {p} | {e} | {n} |\n" for a, p, e, n in sorted(acts))
    REGISTRY.write_text(text)


def show(r: tuple) -> str:
    return f"`{r[0]}` {r[1]} `{r[2]}` `{r[3]}`"


def problems(routes: list, acts: list, reg: dict, tool_names: set, app: str = None) -> list:
    """What fails the check. For one app (`--app`), only its rows count, and a row its code no longer has is news, not
    a failure: the platform's next `--write` takes it out."""
    listed = reg["routes"]
    listed_acts = [tuple(a) for a in reg["actions"]]
    if app:
        listed = [r for r in listed if r[0].startswith(app + ":")]
        listed_acts = [a for a in listed_acts if a[0] == app]
    out = []
    have, known = Counter(routes), Counter(r[:4] for r in listed)
    out += [f"not in the registry: {show(k)}" for k in sorted(have - known)]
    if not app:
        out += [f"in the registry, not in the code: {show(k)}" for k in sorted(known - have)]
    have, known = Counter(map(tuple, acts)), Counter(listed_acts)
    out += [f"visible action without a contract, not in the registry: {' '.join(k)}" for k in sorted(have - known)]
    if not app:
        out += [f"in the registry, has a contract or is gone: {' '.join(k)}" for k in sorted(known - have)]
    for r in listed:
        m = KIND.match(r[4])
        if r[4] == UNCLASSIFIED:
            out.append(f"no kind yet: {show(r)}")
        elif not m or m["op"] and not (CAPABILITY.match(m["op"]) or m["op"] in tool_names):
            out.append(f"not a kind: {show(r)}: {r[4]}")
    if not app:
        debt = sum(r[4] == "debt" for r in listed)
        if debt > (reg["allowed_routes"] or 0):
            out.append(f"{debt} routes in debt, {reg['allowed_routes'] or 0} allowed: the number only falls")
        if len(listed_acts) > (reg["allowed_actions"] or 0):
            out.append(f"{len(listed_acts)} visible actions without a contract, {reg['allowed_actions'] or 0} allowed: the number only falls")
    return out


def main(argv: list) -> int:
    reg = read_registry()
    if "--app" in argv:
        root = Path(argv[argv.index("--app") + 1]).resolve()
        # Its id from its manifest, as a checkout's folder may be named anything; the repository's name otherwise.
        manifest = root / "app.yaml"
        m = re.search(r"^id:\s*([a-z0-9-]+)", manifest.read_text(), re.M) if manifest.exists() else None
        app = m.group(1) if m else root.name.removeprefix("ferrum-app-")
        routes, acts = site_routes(app, root), actions(app, root)
        gone = Counter(r[:4] for r in reg["routes"] if r[0].startswith(app + ":")) - Counter(routes)
        gone_acts = Counter(tuple(a) for a in reg["actions"] if a[0] == app) - Counter(acts)
        for k in sorted(gone):
            print(f"gone from the code, the registry's next --write drops it: {show(k)}")
        for k in sorted(gone_acts):
            print(f"has a contract now or is gone, the registry's next --write drops it: {' '.join(k)}")
        fails = problems(routes, acts, reg, tools(), app)
    else:
        routes = platform_routes()
        acts = []
        for app, root in sorted(apps().items()):
            routes += site_routes(app, root)
            acts += actions(app, root)
        if "--write" in argv:
            write_registry(routes, acts, reg)
            reg = read_registry()
        fails = problems(routes, acts, reg, tools())
    for f in fails:
        print(f)
    kinds = {r[:4]: r[4] for r in reg["routes"]}
    debt = sum(kinds.get(r) == "debt" for r in routes)
    print(f"{len(routes)} routes write outside the gate ({debt} debt), {len(acts)} visible actions lack a contract")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
