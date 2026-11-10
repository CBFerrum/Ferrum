#!/usr/bin/env bash
# The chart against deploy/k8s: lint, the image rule, and a resource-by-resource comparison, with
# the defaults and with the optional components on. Needs helm, kubectl and python3 with pyyaml;
# no cluster, no network. Run from the repo root: bash deploy/helm/test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
chart=deploy/helm/ferrum
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
digest=sha256:0000000000000000000000000000000000000000000000000000000000000000

printf 'image:\n  digest: %s\n' "$digest" > "$tmp/digest.yaml"
printf 'image:\n  tag: abc\neditors:\n  enabled: true\n  image: {tag: abc}\nmetricsEngine:\n  enabled: true\n  image: {tag: abc}\n' > "$tmp/all.yaml"
# The Namespace is rendered here because deploy/k8s has it; with Helm, --create-namespace does it.
tpl() { helm template ferrum "$chart" --namespace ferrum --set namespace.create=true "$@"; }

echo "lint"
helm lint "$chart" -f "$tmp/digest.yaml"

echo "no digest and no tag fails"
if err=$(helm template ferrum "$chart" 2>&1); then echo "rendered without an image version"; exit 1; fi
grep -q 'no default image version' <<<"$err" || { echo "unexpected error: $err"; exit 1; }
if err=$(helm template ferrum "$chart" -f "$tmp/digest.yaml" --set editors.enabled=true 2>&1); then echo "editors rendered without an image version"; exit 1; fi
grep -q 'editors.image' <<<"$err" || { echo "unexpected error: $err"; exit 1; }

echo "a digest wins over a tag"
tpl -f "$tmp/digest.yaml" --set image.tag=abc | grep -q "image: ghcr.io/cbferrum/ferrum@$digest"

echo "defaults against deploy/k8s"
kubectl kustomize deploy/k8s > "$tmp/k.yaml"
tpl -f "$tmp/digest.yaml" > "$tmp/c.yaml"
python3 deploy/helm/compare.py "$tmp/k.yaml" "$tmp/c.yaml"

echo "editors and metrics engine against deploy/k8s with both components"
cp -R deploy/k8s "$tmp/k8s"
sed -i.bak -e 's/^# components:/components:/' -e 's/^#   - editors/  - editors/' -e 's/^#   - metrics-engine/  - metrics-engine/' "$tmp/k8s/kustomization.yaml"
grep -q '^  - metrics-engine' "$tmp/k8s/kustomization.yaml"
kubectl kustomize "$tmp/k8s" > "$tmp/k.yaml"
tpl -f "$tmp/all.yaml" > "$tmp/c.yaml"
python3 deploy/helm/compare.py "$tmp/k.yaml" "$tmp/c.yaml"

echo "migration job is a pre-install,pre-upgrade hook, and off by default"
! tpl -f "$tmp/digest.yaml" | grep -q 'kind: Job'
tpl -f "$tmp/digest.yaml" --set migrate.enabled=true | grep -q 'helm.sh/hook: pre-install,pre-upgrade'

echo "enabling it runs the migrate task and stops the server from migrating itself"
tpl -f "$tmp/digest.yaml" --set migrate.enabled=true > "$tmp/m.yaml"
python3 - "$tmp/m.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
job = next(d for d in docs if d["kind"] == "Job")
assert job["spec"]["template"]["spec"]["containers"][0]["args"] == ["task", "migrate"], job
dep = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum")
env = {e["name"]: e.get("value") for e in dep["spec"]["template"]["spec"]["containers"][0]["env"]}
assert env.get("FERRUM_MIGRATE_AT_START") == "false", env
PY
! tpl -f "$tmp/digest.yaml" | grep -q FERRUM_MIGRATE_AT_START

echo "ok"
