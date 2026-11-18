#!/usr/bin/env bash
# The Kubernetes reference stack: an example PostgreSQL (CloudNativePG, reference/kubernetes/database), an example object store
# (SeaweedFS, store.yaml), and then the product, given their two addresses (deploy/helm/ferrum with ferrum-values.yaml).
# It is not the product, and not a way to run one: README.md says what it gives and what you are then responsible for.
#
#   reference/kubernetes/install.sh -f my-values.yaml [-d database-values.yaml]
#
# -f  values for the product chart, over ferrum-values.yaml (repeatable): at least image.tag or image.digest, publicUrl,
#     gateway.hostname and office.host, which ship as placeholders.
# -d  values for the example database chart, over its own (repeatable).
# It needs kubectl and helm on a cluster that has a Gateway API implementation, cert-manager and metrics-server (what
# deploy/kind/install.sh installs for CI), and installs the CloudNativePG operator and its Barman Cloud plugin itself. Run again,
# it changes what changed. NS is the namespace (default ferrum).
set -euo pipefail
cd "$(dirname "$0")/../.."

NS=${NS:-ferrum}
CNPG=1.30.1
BARMAN_PLUGIN=v0.15.1
product=() database=()
while [ $# -gt 0 ]; do
  case "$1" in
    -f) product+=(-f "${2:?-f needs a file}"); shift 2 ;;
    -d) database+=(-f "${2:?-d needs a file}"); shift 2 ;;
    *) echo "usage: $0 [-f product-values.yaml]... [-d database-values.yaml]..." >&2; exit 2 ;;
  esac
done
[ ${#product[@]} -gt 0 ] || { echo "give the product's values with -f: its image, publicUrl, gateway.hostname and office.host have no usable default" >&2; exit 2; }

step() { echo "==> $*"; }
k() { kubectl -n "$NS" "$@"; }
# Applies stdin until the API accepts it (webhooks answer a little after their pods start).
apply() {
  local input
  input=$(cat)
  for _ in $(seq 1 30); do
    kubectl apply "$@" -f - <<<"$input" && return 0
    sleep 5
  done
  echo "could not apply $*" >&2
  return 1
}

step "CloudNativePG $CNPG and the Barman Cloud plugin $BARMAN_PLUGIN"
kubectl apply --server-side -f "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/release-${CNPG%.*}/releases/cnpg-$CNPG.yaml" >/dev/null
kubectl -n cnpg-system rollout status deploy/cnpg-controller-manager --timeout=5m
curl -fsSL "https://github.com/cloudnative-pg/plugin-barman-cloud/releases/download/$BARMAN_PLUGIN/manifest.yaml" | apply
kubectl -n cnpg-system rollout status deploy/barman-cloud --timeout=5m

step "the namespace and the store's key (made once, kept)"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl label namespace "$NS" pod-security.kubernetes.io/enforce=restricted --overwrite >/dev/null
if ! k get secret ferrum-store-keys >/dev/null 2>&1; then
  k create secret generic ferrum-store-keys --from-literal=access-key="FERRUM$(openssl rand -hex 8 | tr 'a-f' 'A-F')" \
    --from-literal=secret-key="$(openssl rand -hex 24)" >/dev/null
fi
access=$(k get secret ferrum-store-keys -o jsonpath='{.data.access-key}' | base64 -d)
secret=$(k get secret ferrum-store-keys -o jsonpath='{.data.secret-key}' | base64 -d)
# The database's backups go to the same store, signed with the same key.
k create secret generic ferrum-backup-s3 --from-literal=ACCESS_KEY_ID="$access" --from-literal=ACCESS_SECRET_KEY="$secret" \
  --dry-run=client -o yaml | k apply -f - >/dev/null
# The server's own Secret (docs/operations.md, "The platform's secrets"): the store's key, and a metrics token, each only
# where it is not there already; whatever else it holds is left alone.
if ! k get secret ferrum-secrets >/dev/null 2>&1; then
  k create secret generic ferrum-secrets --from-literal=FERRUM_METRICS_TOKEN="$(openssl rand -hex 32)" >/dev/null
fi
k patch secret ferrum-secrets --type merge -p "$(printf '{"stringData":{"AWS_ACCESS_KEY_ID":"%s","AWS_SECRET_ACCESS_KEY":"%s"}}' "$access" "$secret")" >/dev/null

step "the object store"
k create configmap ferrum-store-start --from-file=start.sh=reference/store/start.sh --dry-run=client -o yaml | k apply -f - >/dev/null
k apply -f reference/kubernetes/store.yaml >/dev/null
k rollout status statefulset/ferrum-store --timeout=5m
# The bucket the database's backups go to (the server's own bucket is made by `ferrum task bucket_set_up`, below).
k exec ferrum-store-0 -- sh -c 'echo "s3.bucket.create -name ferrum-backups" | weed shell -master=127.0.0.1:9333' >/dev/null

step "the database (reference/kubernetes/database)"
helm upgrade --install ferrum-database reference/kubernetes/database -n "$NS" \
  --set database.backup.destinationPath=s3://ferrum-backups/ferrum --set database.backup.endpointURL=http://ferrum-store:8333 \
  ${database[@]+"${database[@]}"}
k wait cluster/ferrum-db --for=condition=Ready --timeout=15m

step "the product, given the two addresses (deploy/helm/ferrum, reference/kubernetes/ferrum-values.yaml)"
# Not waited for: the server is not ready until its bucket exists, which `ferrum task bucket_set_up` makes below. Helm itself
# waits for the migrate Job, a hook of the release.
helm upgrade --install ferrum deploy/helm/ferrum -n "$NS" -f reference/kubernetes/ferrum-values.yaml "${product[@]}"
k wait --for=jsonpath='{.status.phase}'=Running pod -l app.kubernetes.io/name=ferrum --timeout=10m
step "the bucket: ferrum task bucket_set_up (versioning, lifecycle rules, encryption; again does nothing)"
k exec deploy/ferrum -- ferrum task bucket_set_up
k rollout status deploy/ferrum --timeout=10m
step "ready"
