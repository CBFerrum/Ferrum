#!/usr/bin/env bash
# Installs what deploy/k8s needs into the current cluster, at the versions CI tests: Envoy Gateway
# (Gateway API), cert-manager with Gateway API support, the CloudNativePG operator and its Barman
# Cloud plugin, and the Prometheus operator's ServiceMonitor/PodMonitor definitions. For CI it also
# adds a self-signed issuer named like the production one and an S3 emulator with the two buckets.
#
#   kind create cluster --config deploy/kind/kind.yaml && deploy/kind/install.sh
set -euo pipefail
cd "$(dirname "$0")"

ENVOY_GATEWAY=v1.9.3
CERT_MANAGER=v1.21.3
CNPG=1.30.1
BARMAN_PLUGIN=v0.15.1
PROMETHEUS_OPERATOR=v0.94.2

step() { echo "==> $*"; }
# Applies stdin or a file until the API accepts it (webhooks answer a little after their pods start).
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

step "Envoy Gateway $ENVOY_GATEWAY (with the Gateway API definitions)"
kubectl apply --server-side -f "https://github.com/envoyproxy/gateway/releases/download/$ENVOY_GATEWAY/install.yaml" >/dev/null
ready envoy-gateway-system envoy-gateway
apply <<'EOF'
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: eg
spec:
  controllerName: gateway.envoyproxy.io/gatewayclass-controller
EOF

step "cert-manager $CERT_MANAGER"
kubectl apply -f "https://github.com/cert-manager/cert-manager/releases/download/$CERT_MANAGER/cert-manager.yaml" >/dev/null
# Certificates for Gateway listeners (cert-manager.io/cluster-issuer on the Gateway).
kubectl -n cert-manager patch deploy cert-manager --type=json \
  -p '[{"op": "add", "path": "/spec/template/spec/containers/0/args/-", "value": "--enable-gateway-api"}]'
for d in cert-manager cert-manager-cainjector cert-manager-webhook; do ready cert-manager "$d"; done
# Named like the production issuer: a CI root authority signs the gateway's certificate.
apply <<'EOF'
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: ci-root
spec:
  selfSigned: {}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: ci-root
  namespace: cert-manager
spec:
  isCA: true
  commonName: Ferrum CI root
  secretName: ci-root
  privateKey: {algorithm: ECDSA, size: 256}
  issuerRef: {name: ci-root, kind: ClusterIssuer, group: cert-manager.io}
---
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: letsencrypt
spec:
  ca:
    secretName: ci-root
EOF
kubectl wait clusterissuer/letsencrypt --for=condition=Ready --timeout=3m

step "CloudNativePG $CNPG and the Barman Cloud plugin $BARMAN_PLUGIN"
kubectl apply --server-side -f "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/release-${CNPG%.*}/releases/cnpg-$CNPG.yaml" >/dev/null
ready cnpg-system cnpg-controller-manager
curl -fsSL "https://github.com/cloudnative-pg/plugin-barman-cloud/releases/download/$BARMAN_PLUGIN/manifest.yaml" | apply
ready cnpg-system barman-cloud

step "Prometheus operator $PROMETHEUS_OPERATOR definitions"
for crd in servicemonitors podmonitors; do
  kubectl apply --server-side -f "https://raw.githubusercontent.com/prometheus-operator/prometheus-operator/$PROMETHEUS_OPERATOR/example/prometheus-operator-crd/monitoring.coreos.com_$crd.yaml" >/dev/null
done

step "S3 emulator"
kubectl apply -f s3.yaml >/dev/null
ready ci-s3 s3
kubectl -n ci-s3 exec deploy/s3 -- python -c "
import boto3
s3 = boto3.client('s3', endpoint_url='http://localhost:5000', region_name='us-east-1', aws_access_key_id='test', aws_secret_access_key='test')
for b in ('ferrum-files', 'ferrum-backups'):
    s3.create_bucket(Bucket=b)
"
step "ready"
