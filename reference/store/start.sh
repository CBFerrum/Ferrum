#!/bin/sh
# SeaweedFS in one process as an S3 store (chosen by trial: docs/references/object-stores.md): S3 on the network, its
# master, volume server and filer inside the container alone.
#
# The access key is the one it is given, STORE_ACCESS_KEY and STORE_SECRET_KEY (it refuses to start without them, naming
# them); the key that encrypts what the store keeps (SSE-S3) is made once, in the store's own data directory. The store
# makes no bucket (-s3.autoCreateBucket=false): `ferrum task bucket_set_up` does, so a wrong bucket name is refused and
# the server is not ready, saying so.
#
# Settings, for running it elsewhere (scripts/object_store_rehearsal.sh runs it with the store's own binary):
# STORE_ACCESS_KEY and STORE_SECRET_KEY (required), STORE_DATA (default /data), STORE_BIND (default 0.0.0.0), STORE_PORT
# (default 8333), STORE_ARGS (more flags for `weed server`).
set -eu
: "${STORE_ACCESS_KEY:?set STORE_ACCESS_KEY and STORE_SECRET_KEY: the key Ferrum signs its requests to this store with}"
: "${STORE_SECRET_KEY:?set STORE_ACCESS_KEY and STORE_SECRET_KEY: the key Ferrum signs its requests to this store with}"
data=${STORE_DATA:-/data}
conf=$data/ferrum
umask 077
mkdir -p "$conf"
if [ ! -f "$conf/kek" ]; then
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$conf/kek.new"
  mv "$conf/kek.new" "$conf/kek"
fi
# Written at each start from the two variables: a key changed in the environment is the key from the next start on.
printf '{"identities":[{"name":"ferrum","credentials":[{"accessKey":"%s","secretKey":"%s"}],"actions":["Admin","Read","Write","List","Tagging"]}]}\n' \
  "$STORE_ACCESS_KEY" "$STORE_SECRET_KEY" >"$conf/s3.json"
WEED_S3_SSE_KEK=$(cat "$conf/kek")
export WEED_S3_SSE_KEK
# shellcheck disable=SC2086 # STORE_ARGS is a list of flags
exec weed server -dir="$data" -ip=127.0.0.1 -ip.bind=127.0.0.1 -s3 -s3.ip.bind="${STORE_BIND:-0.0.0.0}" -s3.port="${STORE_PORT:-8333}" \
  -s3.config="$conf/s3.json" -s3.autoCreateBucket=false -s3.port.iceberg=0 -s3.port.lance=0 -filer.encryptVolumeData -volume.max=0 \
  -master.volumeSizeLimitMB=1024 ${STORE_ARGS:-}
