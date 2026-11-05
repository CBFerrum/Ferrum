#!/usr/bin/env bash
# Installs what deploy/k8s needs into a cluster without internet, from a bundle made with
# `deploy/offline/bundle.py --kubernetes` and loaded with `load.py --registry` (docs/operations.md,
# "A site without internet"): the manifests deploy/kind/install.sh would download, at the same
# versions, applied from the bundle. Certificates come from the site's own authority, not ACME: put
# its certificate and key in the secret site-ca first, and the ClusterIssuer the Gateway names
# (letsencrypt) signs with it.
#
#   kubectl create namespace cert-manager
#   kubectl -n cert-manager create secret tls site-ca --cert=ca.crt --key=ca.key
#   deploy/offline/install.sh [--kubelet-insecure-tls]
set -euo pipefail
cd "$(dirname "$0")/../.."
manifests=manifests
[[ -f bundle.json && -d $manifests ]] || { echo "run this from a bundle made with bundle.py --kubernetes" >&2; exit 1; }
insecure_kubelets=false
[[ ${1:-} == --kubelet-insecure-tls ]] && insecure_kubelets=true

step() { echo "==> $*"; }
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
ready() { kubectl -n "$1" rollout status "deploy/$2" --timeout=5m; }

step "Envoy Gateway (with the Gateway API definitions)"
kubectl apply --server-side -f "$manifests/envoy-gateway.yaml" >/dev/null
ready envoy-gateway-system envoy-gateway
apply <<'EOF'
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: eg
spec:
  controllerName: gateway.envoyproxy.io/gatewayclass-controller
EOF

step "cert-manager, issuing from the site's authority"
kubectl apply -f "$manifests/cert-manager.yaml" >/dev/null
kubectl -n cert-manager patch deploy cert-manager --type=json \
  -p '[{"op": "add", "path": "/spec/template/spec/containers/0/args/-", "value": "--enable-gateway-api"}]'
for d in cert-manager cert-manager-cainjector cert-manager-webhook; do ready cert-manager "$d"; done
if ! kubectl -n cert-manager get secret site-ca >/dev/null 2>&1; then
  echo "create the secret site-ca with the site's certificate authority first:" >&2
  echo "  kubectl -n cert-manager create secret tls site-ca --cert=ca.crt --key=ca.key" >&2
  exit 1
fi
# Named as the Gateway names its issuer (deploy/k8s/gateway.yaml); browsers must trust the site's authority.
apply <<'EOF'
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: letsencrypt
spec:
  ca:
    secretName: site-ca
EOF
kubectl wait clusterissuer/letsencrypt --for=condition=Ready --timeout=3m

step "CloudNativePG and the Barman Cloud plugin"
kubectl apply --server-side -f "$manifests/cloudnative-pg.yaml" >/dev/null
ready cnpg-system cnpg-controller-manager
apply < "$manifests/barman-cloud.yaml"
ready cnpg-system barman-cloud

step "metrics-server"
kubectl apply -f "$manifests/metrics-server.yaml" >/dev/null
if $insecure_kubelets; then
  kubectl -n kube-system patch deploy metrics-server --type=json \
    -p '[{"op": "add", "path": "/spec/template/spec/containers/0/args/-", "value": "--kubelet-insecure-tls"}]'
fi
ready kube-system metrics-server

step "Prometheus operator definitions (ServiceMonitor, PodMonitor)"
kubectl apply --server-side -f "$manifests/servicemonitors.yaml" -f "$manifests/podmonitors.yaml" >/dev/null

step "ready: edit deploy/k8s as docs/operations.md says, then kubectl apply -k deploy/k8s"
