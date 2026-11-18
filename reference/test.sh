#!/usr/bin/env bash
# The reference stacks (reference/README.md): every repository path a README here names exists, the scripts parse, and the
# single-host stack merges with the product's Compose file (docker compose validates a file without a daemon). Run from the
# repo root: bash reference/test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

echo "every path a README names exists"
python3 - <<'PY'
import pathlib, re, sys
missing = []
for readme in sorted(pathlib.Path("reference").rglob("README.md")):
    for name in re.findall(r"`([^`\s]+)`", readme.read_text()):
        name = name.strip(".,;:()")
        # Repository paths only: not placeholders, globs, commands, or Kubernetes/Docker names.
        if not re.match(r"^(deploy|reference|docs|scripts)/[A-Za-z0-9_./-]+$", name) or "<" in name or "*" in name:
            continue
        # (The .env is written by up.sh when it first runs.)
        if not pathlib.Path(name).exists() and not name.endswith("/.env"):
            missing.append(f"{readme}: {name}")
for relative in re.findall(r"\]\(([^)#]+)\)", pathlib.Path("reference/README.md").read_text()):
    if not (pathlib.Path("reference") / relative).exists():
        missing.append(f"reference/README.md: {relative}")
if missing:
    sys.exit("named and not there:\n  " + "\n  ".join(missing))
PY

echo "the scripts parse"
for f in reference/single-host/up.sh reference/single-host/smoke.sh reference/kubernetes/install.sh reference/kubernetes/smoke.sh reference/kubernetes/scale-database.sh; do
  bash -n "$f"
done
sh -n reference/store/start.sh
sh -n reference/single-host/postgres-init/01-ferrum-role.sh
echo "the store refuses to start without its key, naming it"
if err=$(env -u STORE_ACCESS_KEY -u STORE_SECRET_KEY sh reference/store/start.sh 2>&1); then echo "started without a key"; exit 1; fi
grep -q 'STORE_ACCESS_KEY' <<<"$err" || { echo "unexpected: $err"; exit 1; }

echo "the YAML parses, and the single-host stack merges with the product's Compose file"
python3 - <<'PY'
import yaml
for path in ("reference/kubernetes/store.yaml", "reference/kubernetes/restore.yaml", "reference/kubernetes/ferrum-values.yaml", "reference/single-host/compose.yml"):
    list(yaml.safe_load_all(open(path)))
PY
if docker compose version >/dev/null 2>&1; then
  merged=$(FERRUM_IMAGE=ferrum:test docker compose -f deploy/docker-compose.yml -f reference/single-host/compose.yml --env-file reference/single-host/.env.example config)
  python3 - "$merged" <<'PY'
import sys, yaml
services = yaml.safe_load(sys.argv[1])["services"]
assert {"ferrum", "pdf", "office", "db", "store", "store-setup", "backup"} <= set(services), sorted(services)
env = services["ferrum"]["environment"]
assert env["DATABASE_URL"].endswith("@db:5432/ferrum") and env["FERRUM_S3_ENDPOINT"] == "http://store:8333", env
assert env["FERRUM_BACKUP_REACH_DAYS"] == "30" == env["FERRUM_FILES_DELETE_AFTER_DAYS"], "the dumps' reach is what the server is told"
assert set(services["ferrum"]["depends_on"]) == {"db", "store"}
assert services["store-setup"]["command"] == ["task", "bucket_set_up"]
PY
  # The product's own file runs nothing that holds state, and takes the two addresses from outside.
  product=$(FERRUM_IMAGE=ferrum:test docker compose -f deploy/docker-compose.yml config)
  ! grep -qE 'postgres|seaweedfs' <<<"$product" || { echo "the product's Compose file names a database or a store"; exit 1; }
else
  echo "    (docker compose is not here: the Compose files were only parsed)"
fi
echo "ok"
