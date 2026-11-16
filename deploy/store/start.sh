#!/bin/sh
# The object store deploy/docker-compose.yml bundles: SeaweedFS in one process (chosen by trial:
# docs/references/object-stores.md).
#
# Its first start makes, in the store's own volume, an access key for the Ferrum server and the key that encrypts what
# the store keeps (SSE-S3), and hands the access key to the server once, in a volume only the two share
# (keys.json): the server keeps it in its vault and deletes the file. Nothing is typed into deploy/.env. The handoff
# also asks the server to make the bucket, with versioning, lifecycle rules and default encryption.
# Then the store runs: S3 on the network, its master, volume server and filer inside the container alone.
#
# With STORE_S3_CONFIG (deploy/store-files.yml), the access keys come from the server's vault instead: the ones this
# store handed over, rendered for each start into that file and rotated by the vault since (`ferrum task render
# service:object-store`; docs/operations.md, "Files for the programs beside the server"). Nothing is made or handed over
# here then but the encryption key, where there is none yet.
#
# Settings, for running it elsewhere (scripts/object_store_rehearsal.sh runs it with the store's own binary):
# STORE_DATA (default /data), STORE_HANDOFF (default /handoff), STORE_BIND (default 0.0.0.0), STORE_PORT (default
# 8333), STORE_OWNER (the user the server runs as, default 10001), STORE_ARGS (more flags for `weed server`),
# STORE_S3_CONFIG (the rendered keys, above).
set -eu
data=${STORE_DATA:-/data}
handoff=${STORE_HANDOFF:-/handoff}
conf=$data/ferrum
umask 077
hex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }
s3=$conf/s3.json
if [ -n "${STORE_S3_CONFIG:-}" ]; then
  [ -f "$STORE_S3_CONFIG" ] || { echo "no $STORE_S3_CONFIG: the store's keys are rendered before it starts (ferrum task render service:object-store)" >&2; exit 1; }
  s3=$STORE_S3_CONFIG
  mkdir -p "$conf"
  if [ ! -f "$conf/kek" ]; then
    hex 32 >"$conf/kek.new"
    mv "$conf/kek.new" "$conf/kek"
  fi
elif [ ! -f "$conf/s3.json" ]; then
  mkdir -p "$conf" "$handoff"
  id=FERRUM$(hex 10 | tr 'a-f' 'A-F')
  secret=$(hex 24)
  hex 32 >"$conf/kek"
  printf '{"identities":[{"name":"ferrum","credentials":[{"accessKey":"%s","secretKey":"%s"}],"actions":["Admin","Read","Write","List","Tagging"]}]}\n' \
    "$id" "$secret" >"$conf/s3.json.new"
  # set_up_bucket: this store is the installation's alone, so the server makes the bucket with versioning, lifecycle
  # rules and encryption (a store of an installation's own is never changed unasked).
  printf '{"aws_access_key_id":"%s","aws_secret_access_key":"%s","set_up_bucket":true}\n' "$id" "$secret" >"$handoff/keys.json.new"
  # The server reads the file and then deletes it: its user owns both (as root here), or, run as one user, it is that user.
  chown "${STORE_OWNER:-10001}" "$handoff" "$handoff/keys.json.new" 2>/dev/null || true
  mv "$handoff/keys.json.new" "$handoff/keys.json"
  # Last: a start stopped before this line makes everything again, the handed-over keys included.
  mv "$conf/s3.json.new" "$conf/s3.json"
fi
WEED_S3_SSE_KEK=$(cat "$conf/kek")
export WEED_S3_SSE_KEK
# No bucket made by a write that names it (-s3.autoCreateBucket=false): only the server's set-up makes one, so a wrong
# bucket name is refused, and the server is not ready, saying so.
# shellcheck disable=SC2086 # STORE_ARGS is a list of flags
exec weed server -dir="$data" -ip=127.0.0.1 -ip.bind=127.0.0.1 -s3 -s3.ip.bind="${STORE_BIND:-0.0.0.0}" -s3.port="${STORE_PORT:-8333}" \
  -s3.config="$s3" -s3.autoCreateBucket=false -s3.port.iceberg=0 -s3.port.lance=0 -filer.encryptVolumeData -volume.max=0 \
  -master.volumeSizeLimitMB=1024 ${STORE_ARGS:-}
