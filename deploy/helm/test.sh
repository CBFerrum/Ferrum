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

# The online editor is a second kustomization (its own namespace): the two are compared with the chart together.
kust() { { kubectl kustomize "$1"; echo ---; kubectl kustomize "$1/office"; } > "$2"; }

echo "defaults against deploy/k8s and deploy/k8s/office"
kust deploy/k8s "$tmp/k.yaml"
tpl -f "$tmp/digest.yaml" > "$tmp/c.yaml"
python3 deploy/helm/compare.py "$tmp/k.yaml" "$tmp/c.yaml"

echo "editors and metrics engine against deploy/k8s with both components"
cp -R deploy/k8s "$tmp/k8s"
sed -i.bak -e 's/^# components:/components:/' -e 's/^#   - editors/  - editors/' -e 's/^#   - metrics-engine/  - metrics-engine/' "$tmp/k8s/kustomization.yaml"
grep -q '^  - metrics-engine' "$tmp/k8s/kustomization.yaml"
cp -R deploy/k8s/office "$tmp/k8s/office"
kust "$tmp/k8s" "$tmp/k.yaml"
tpl -f "$tmp/all.yaml" > "$tmp/c.yaml"
python3 deploy/helm/compare.py "$tmp/k.yaml" "$tmp/c.yaml"

echo "the online editor is on by default, and one switch takes it out of the chart and the server's settings"
tpl -f "$tmp/digest.yaml" > "$tmp/on.yaml"
grep -q 'FERRUM_OFFICE_URL' "$tmp/on.yaml" && grep -q 'name: ferrum-office' "$tmp/on.yaml" && grep -q 'name: office-https' "$tmp/on.yaml"
tpl -f "$tmp/digest.yaml" --set office.enabled=false > "$tmp/off.yaml"
! grep -qE 'ferrum-office|FERRUM_OFFICE|office-https' "$tmp/off.yaml"
echo "no PodDisruptionBudget for one editor replica, one for two"
! grep -q 'name: ferrum-office' <(python3 - "$tmp/on.yaml" <<'PY'
import sys, yaml
for d in yaml.safe_load_all(open(sys.argv[1])):
    if d and d["kind"] == "PodDisruptionBudget": print("name:", d["metadata"]["name"])
PY
)
tpl -f "$tmp/digest.yaml" --set office.replicas=2 | grep -A3 'kind: PodDisruptionBudget' | grep -q 'name: ferrum-office'
echo "the editor accepts one WOPI host, has no console, and runs as a non-root user"
python3 - "$tmp/on.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
dep = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum-office")
c = dep["spec"]["template"]["spec"]["containers"][0]
env = {e["name"]: e["value"] for e in c["env"]}
assert env["aliasgroup1"] == "http://ferrum.ferrum.svc.cluster.local:80", env
assert "*" not in env["aliasgroup1"], env
assert "admin_console.enable=false" in env["extra_params"] and "net.frame_ancestors=https://erp.example.com" in env["extra_params"], env
assert dep["spec"]["template"]["spec"]["securityContext"]["runAsNonRoot"] is True
assert c["securityContext"]["capabilities"]["drop"] == ["ALL"]
assert c["resources"]["limits"]["memory"] and c["resources"]["requests"]["memory"]
assert c["readinessProbe"] and c["livenessProbe"]
srv = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "ferrum")
senv = {e["name"]: e.get("value") for e in srv["spec"]["template"]["spec"]["containers"][0]["env"]}
assert senv["FERRUM_OFFICE_ORIGIN"] == "https://office.example.com", senv
PY

echo "migration job is a pre-install,pre-upgrade hook, and off by default"
! tpl -f "$tmp/digest.yaml" | grep -q 'kind: Job'
tpl -f "$tmp/digest.yaml" --set migrate.enabled=true --is-upgrade | grep -q 'helm.sh/hook: pre-install,pre-upgrade'
tpl -f "$tmp/digest.yaml" --set migrate.enabled=true,database.enabled=false,database.urlSecret.name=db | grep -q 'helm.sh/hook: pre-install,pre-upgrade'

echo "on the first install with the chart's own database (not there yet when hooks run), the pods migrate themselves"
! tpl -f "$tmp/digest.yaml" --set migrate.enabled=true | grep -qE 'kind: Job|FERRUM_MIGRATE_AT_START'

echo "enabling it runs the migrate task and stops the server from migrating itself"
tpl -f "$tmp/digest.yaml" --set migrate.enabled=true --is-upgrade > "$tmp/m.yaml"
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

echo "an ExternalSecret makes the server's Secret only when asked, and needs its store"
! tpl -f "$tmp/digest.yaml" | grep -q 'kind: ExternalSecret'
tpl -f "$tmp/digest.yaml" --set secrets.externalSecret.enabled=true --set secrets.externalSecret.secretStoreRef.name=vault \
  --set-json 'secrets.externalSecret.dataFrom=[{"extract":{"key":"ferrum"}}]' | grep -A10 'kind: ExternalSecret' | grep -q 'name: ferrum-secrets'
if err=$(tpl -f "$tmp/digest.yaml" --set secrets.externalSecret.enabled=true 2>&1); then echo "rendered an ExternalSecret without a store"; exit 1; fi
grep -q 'secretStoreRef.name' <<<"$err"

echo "ok"
