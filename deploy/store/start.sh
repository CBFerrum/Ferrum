#!/bin/sh
# The object store deploy/docker-compose.yml bundles: SeaweedFS in one process (chosen by trial:
# docs/references/object-stores.md).
#
# Its first start makes, in the store's own volume, an access key for the Ferrum server and the key that encrypts what
# the store keeps (SSE-S3), and hands the access key to the server once, in a volume only the two share
# (keys.json): the server keeps it in its vault and deletes the file. Nothing is typed into deploy/.env. The server
# makes the bucket, with versioning, lifecycle rules and default encryption (FERRUM_S3_SET_UP_BUCKET).
# Then the store runs: S3 on the network, its master, volume server and filer inside the container alone.
#
# Settings, for running it elsewhere (scripts/object_store_rehearsal.sh runs it with the store's own binary):
# STORE_DATA (default /data), STORE_HANDOFF (default /handoff), STORE_BIND (default 0.0.0.0), STORE_PORT (default
# 8333), STORE_OWNER (the user the server runs as, default 10001), STORE_ARGS (more flags for `weed server`).
set -eu
data=${STORE_DATA:-/data}
handoff=${STORE_HANDOFF:-/handoff}
conf=$data/ferrum
umask 077
hex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }
if [ ! -f "$conf/s3.json" ]; then
  mkdir -p "$conf" "$handoff"
  id=FERRUM$(hex 10 | tr 'a-f' 'A-F')
  secret=$(hex 24)
  hex 32 >"$conf/kek"
  printf '{"identities":[{"name":"ferrum","credentials":[{"accessKey":"%s","secretKey":"%s"}],"actions":["Admin","Read","Write","List","Tagging"]}]}\n' \
    "$id" "$secret" >"$conf/s3.json.new"
  printf '{"aws_access_key_id":"%s","aws_secret_access_key":"%s"}\n' "$id" "$secret" >"$handoff/keys.json.new"
  # The server reads the file and then deletes it: its user owns both (as root here), or, run as one user, it is that user.
  chown "${STORE_OWNER:-10001}" "$handoff" "$handoff/keys.json.new" 2>/dev/null || true
  mv "$handoff/keys.json.new" "$handoff/keys.json"
  # Last: a start stopped before this line makes everything again, the handed-over keys included.
  mv "$conf/s3.json.new" "$conf/s3.json"
fi
WEED_S3_SSE_KEK=$(cat "$conf/kek")
export WEED_S3_SSE_KEK
# shellcheck disable=SC2086 # STORE_ARGS is a list of flags
exec weed server -dir="$data" -ip=127.0.0.1 -ip.bind=127.0.0.1 -s3 -s3.ip.bind="${STORE_BIND:-0.0.0.0}" -s3.port="${STORE_PORT:-8333}" \
  -s3.config="$conf/s3.json" -s3.port.iceberg=0 -s3.port.lance=0 -filer.encryptVolumeData -volume.max=0 -master.volumeSizeLimitMB=1024 \
  ${STORE_ARGS:-}
