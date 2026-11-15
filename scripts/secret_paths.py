#!/usr/bin/env python3
"""Who may name what opens secrets (crates/kernel/src/vault; docs/security.md, "The vault"; the design's D4).

    scripts/secret_paths.py            # CI runs this: fails on a use it does not allow, or one the baseline lacks
    scripts/secret_paths.py --write    # writes the baseline from the code; it refuses to let the baseline grow

Inside one process the vault keeps owners apart by the shape of its code; this keeps the rest of the code to that
shape. It reads the sources (not the tests) of this repository's crates and server, and of every app the lock compiles
in (`cargo metadata`, as scripts/mutation_paths.py reads them), and checks:

* the vault's tables and its setting (`vault.root_slot`, `vault.key`, `vault.secret`, `vault.use_count`,
  `vault.event`, `vault.transfer`, `ferrum.vault`) are named only by the kernel's vault module, its migrations, and the
  transaction `Db::vault_tx` opens for it (packages' SQL is read too);
* no app calls the ways into the vault that are not its own handle: opening it, and the functions for people and
  operators that name an owner (the platform's code, the kernel and the server, calls them);
* no app has `unsafe` code, nor a cryptography crate of its own (one that keys, seals or signs: hashes alone are not
  counted), beyond what the baseline lists from before;
* the master key (`secret_key`, `SecretKey`, `FERRUM_SECRET_KEY`, `open_sealed`, ...) is named outside the vault only
  where the baseline says, no more often: the baseline may only shrink, and the slice that moves the stores into the
  vault empties it.
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "docs/registry/secret-paths.md"
# The vault's own module, and the transaction the kernel's database layer opens for it.
VAULT = ("crates/kernel/src/vault/",)
VAULT_TX = (
    "crates/kernel/src/db.rs",
    "crates/kernel/migrations/20261119000201_vault.sql",
    "crates/kernel/migrations/20261119000202_vault_event.sql",
    "crates/kernel/migrations/20261119000401_vault_transfer.sql",
)
# Apps in this workspace (the rest come from their own repositories): their packages are compiled in as apps.
WORKSPACE_APPS = {"ferrum-studio", "ferrum-ops"}

TABLES = re.compile(r"\bvault\.(root_slot|key|secret|use_count|event|transfer)\b|\bferrum\.vault\b")
WAYS_IN = re.compile(
    r"\bvault::(open|rewrap|put_slot|remove_slot|rotate_installation_key|events)\s*\(|\bvault::(\{[^}]*\b)?Opener\b"
    r"|\.(vault_set|vault_rotate|vault_remove|vault_secrets|vault_regenerate|vault_reseal|rotate_owner_key|rotate_workspace_key)\s*\("
)
UNSAFE = re.compile(r"\bunsafe\s*(\{|fn\b|impl\b|trait\b|extern\b)")
MASTER = re.compile(
    r"\bsecret_key\b|\bretired_secret_keys\b|\bSecretKey\b|\bFERRUM_SECRET_KEY(_COMMAND)?\b|\bFERRUM_RETIRED_SECRET_KEYS\b"
    r"|\bopen_sealed\b|\bsecret_keys\(\)|\bseal_webhook_secret\b"
)
# Crates that key, seal or sign. Hash functions alone (sha1, sha2, sha3, md-5, blake3) are not counted.
CRYPTO = {
    "aead", "aes", "aes-gcm", "aes-gcm-siv", "aes-siv", "argon2", "bcrypt", "chacha20", "chacha20poly1305", "cipher",
    "crypto_box", "ecdsa", "ed25519-dalek", "hkdf", "hmac", "jsonwebtoken", "k256", "libsodium-sys", "openssl", "orion",
    "p256", "p384", "pbkdf2", "ring", "rsa", "scrypt", "sodiumoxide", "x25519-dalek", "xsalsa20poly1305",
}
STRING = re.compile(r'r(#*)"(.*?)"\1|"((?:[^"\\]|\\.)*)"', re.S)


def code_lines(text: str):
    """(line number, line) of what is not a test: a file's lines up to its `#[cfg(test)]`, comment lines left out."""
    for n, line in enumerate(text.split("\n"), 1):
        if re.match(r"\s*#\[cfg\(test\)\]", line):
            return
        if not line.lstrip().startswith(("//", "--")):
            yield n, line


def strings(text: str) -> list:
    """The string literals in `text`, as written."""
    return [m.group(2) if m.group(2) is not None else m.group(3) for m in STRING.finditer(text)]


def outside_strings(line: str) -> str:
    return STRING.sub('""', line)


def sources(root: Path):
    """Each source file (Rust, SQL) of a crate or repository that is not a test, an example or a build artefact."""
    for path in sorted([*root.rglob("*.rs"), *root.rglob("*.sql")]):
        parts = path.relative_to(root).parts
        if {"tests", "target", "examples", "benches", "third_party", "node_modules"} & set(parts):
            continue
        yield path


def packages() -> list:
    """(name, its root, whether it is an app, its direct dependencies) for this workspace's members and every
    CBFerrum package the lock compiles in."""
    run = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"], cwd=ROOT, capture_output=True, text=True)
    if run.returncode != 0:
        sys.exit(f"cargo metadata: {run.stderr.strip()}")
    meta = json.loads(run.stdout)
    members = set(meta["workspace_members"])
    out = []
    for p in meta["packages"]:
        source = p.get("source") or ""
        mine = p["id"] in members
        if not mine and "github.com/CBFerrum/" not in source:
            continue
        deps = sorted({d["name"] for d in p["dependencies"] if d.get("kind") in (None, "normal")})
        out.append((p["name"], Path(p["manifest_path"]).parent, not mine or p["name"] in WORKSPACE_APPS, deps))
    # The packages' SQL, which the schema sync runs as the server's role.
    out.append(("packages", ROOT / "packages", False, []))
    return out


def where(name: str, root: Path, path: Path) -> str:
    """How a file is named in the baseline: by its place in this repository, or by its app package and place in it."""
    try:
        return path.relative_to(ROOT).as_posix()
    except ValueError:
        return f"{name}:{path.relative_to(root).as_posix()}"


def scan(pkgs: list) -> tuple:
    """What the code does: (refusals, master-key uses by file, (app, crate) cryptography, unsafe by file)."""
    refusals, master, unsafe = [], {}, {}
    crypto = set()
    seen = set()
    for name, root, app, deps in pkgs:
        if app:
            crypto |= {(name, d) for d in deps if d in CRYPTO}
        # A package inside another's directory (a workspace's members) is read once, as its own.
        nested = [r for n, r, _, _ in pkgs if r != root and root in r.parents]
        for path in sources(root):
            if any(n in path.parents for n in nested) or path in seen:
                continue
            seen.add(path)
            at = where(name, root, path)
            text = path.read_text(errors="replace")
            in_vault = at.startswith(VAULT)
            lines = list(code_lines(text))
            for n, line in lines:
                # The whole line, not only its string literals: SQL goes on over lines a literal began on before.
                if not in_vault and not at.startswith(VAULT_TX) and TABLES.search(line):
                    refusals.append(f"{at}:{n}: names the vault's tables or its setting, which only its module does")
                bare = outside_strings(line)
                if app and WAYS_IN.search(bare):
                    refusals.append(f"{at}:{n}: an app calls a way into the vault that is not its own handle")
                if app and UNSAFE.search(bare):
                    unsafe[at] = unsafe.get(at, 0) + 1
                if not in_vault:
                    found = len(MASTER.findall(bare)) + sum(len(MASTER.findall(s)) for s in strings(line))
                    if found:
                        master[at] = master.get(at, 0) + found
    return refusals, master, crypto, unsafe


def read_baseline() -> tuple:
    master, crypto, unsafe = {}, set(), {}
    if not BASELINE.exists():
        return master, crypto, unsafe
    section = None
    for line in BASELINE.read_text().split("\n"):
        if line.startswith("## "):
            section = line[3:].strip()
            continue
        m = re.match(r"^\| `([^`]+)` \| (\S+) \|$", line)
        if not m:
            continue
        if section == "The master key outside the vault":
            master[m.group(1)] = int(m.group(2))
        elif section == "Cryptography crates in apps":
            crypto.add((m.group(1), m.group(2).strip("`")))
        elif section == "Unsafe code in apps":
            unsafe[m.group(1)] = int(m.group(2))
    return master, crypto, unsafe


def write_baseline(master: dict, crypto: set, unsafe: dict) -> None:
    rows = lambda pairs: "\n".join(pairs) if pairs else "| (none) | |"
    text = f"""# Secret paths

What may name what opens secrets, outside the vault (crates/kernel/src/vault; docs/security.md, "The vault").
`scripts/secret_paths.py` keeps it true: CI fails on a use outside the vault that is not listed here, on more uses
in a file than listed, and on any app that names the vault's tables, opens it, or calls the people's and operators'
functions. `--write` rewrites these tables from the code, and refuses to add to them: they may only shrink. The slice
that moves the existing stores into the vault empties the first.

## The master key outside the vault

`FERRUM_SECRET_KEY` and what reads it, by file (an app's as `package:path`), not counting tests and comments.

| Where | Uses |
|---|---|
{rows([f"| `{k}` | {v} |" for k, v in sorted(master.items())])}

## Cryptography crates in apps

An app's own key-holding cryptography, from before the vault: each to move onto the vault's verbs (derive, seal,
sign) or to stay where a third party's scheme needs it (a government's RSA, a signature format).

| App | Crate |
|---|---|
{rows([f"| `{a}` | `{c}` |" for a, c in sorted(crypto)])}

## Unsafe code in apps

| Where | Blocks |
|---|---|
{rows([f"| `{k}` | {v} |" for k, v in sorted(unsafe.items())])}
"""
    BASELINE.write_text(text)


def main() -> int:
    refusals, master, crypto, unsafe = scan(packages())
    base_master, base_crypto, base_unsafe = read_baseline()
    grew = [f"{k}: names the master key {v} times, the baseline {base_master.get(k, 0)}" for k, v in sorted(master.items()) if v > base_master.get(k, 0)]
    grew += [f"{a} uses the cryptography crate {c} itself, which the baseline does not list" for a, c in sorted(crypto - base_crypto)]
    grew += [f"{k}: {v} unsafe blocks in an app, the baseline {base_unsafe.get(k, 0)}" for k, v in sorted(unsafe.items()) if v > base_unsafe.get(k, 0)]
    if "--write" in sys.argv:
        if grew and BASELINE.exists():
            for g in grew:
                print(f"refused, the baseline only shrinks: {g}")
            return 1
        write_baseline(master, crypto, unsafe)
        print(f"wrote {BASELINE.relative_to(ROOT)}")
        base_master, base_crypto, base_unsafe = master, crypto, unsafe
        grew = []
    for r in refusals:
        print(f"refused: {r}")
    for g in grew:
        print(f"not in the baseline: {g}")
    shrunk = [k for k, v in base_master.items() if master.get(k, 0) < v] + [f"{a} {c}" for a, c in sorted(base_crypto - crypto)]
    if shrunk:
        print(f"the baseline can shrink ({', '.join(shrunk[:5])}{', ...' if len(shrunk) > 5 else ''}): run scripts/secret_paths.py --write")
    print(f"{sum(master.values())} uses of the master key in {len(master)} files outside the vault; {len(crypto)} cryptography crates in apps; {len(refusals)} refused")
    return 1 if refusals or grew else 0


if __name__ == "__main__":
    sys.exit(main())
