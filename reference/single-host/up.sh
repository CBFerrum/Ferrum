#!/usr/bin/env bash
# One command for the single-host reference stack: Ferrum with a PostgreSQL, an object store and a daily dump of the database
# beside it. An example, not the product (README.md says what you are then responsible for).
#
#   FERRUM_IMAGE=ghcr.io/cbferrum/ferrum:<commit> reference/single-host/up.sh [docker compose arguments, default: up -d]
#
# The first run writes reference/single-host/.env (passwords and the store's key made once with openssl, kept; the image and
# PUBLIC_URL as given) and keeps it: a later run reads it. It then runs
#   docker compose -f deploy/docker-compose.yml -f reference/single-host/compose.yml --env-file reference/single-host/.env <arguments>
# with the product's file first, so that the reference's values win the merge. The server is not ready until the one-shot
# `store-setup` has made the bucket, which it does as soon as the store answers.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
env_file=$here/.env

if [ ! -f "$env_file" ]; then
  : "${FERRUM_IMAGE:?set FERRUM_IMAGE to a commit tag (docs/operations.md, Images): it is written into the .env}"
  command -v openssl >/dev/null || { echo "needs openssl, to make the passwords and the store's key" >&2; exit 2; }
  db_password=$(openssl rand -hex 16)
  superuser_password=$(openssl rand -hex 16)
  umask 077
  {
    echo "FERRUM_IMAGE=$FERRUM_IMAGE"
    echo "PUBLIC_URL=${PUBLIC_URL:-http://localhost:8080}"
    echo "POSTGRES_PASSWORD=$superuser_password"
    echo "FERRUM_DB_PASSWORD=$db_password"
    echo "DATABASE_URL=postgres://ferrum:$db_password@db:5432/ferrum"
    echo "FERRUM_S3_BUCKET=ferrum"
    echo "FERRUM_S3_ENDPOINT=http://store:8333"
    echo "FERRUM_S3_REGION=us-east-1"
    echo "AWS_ACCESS_KEY_ID=FERRUM$(openssl rand -hex 8 | tr 'a-f' 'A-F')"
    echo "AWS_SECRET_ACCESS_KEY=$(openssl rand -hex 24)"
    echo "BACKUP_KEEP_DAYS=${BACKUP_KEEP_DAYS:-30}"
    echo "TEMPORAL_PG_HOST=db"
    echo "TEMPORAL_PG_USER=postgres"
    echo "TEMPORAL_PG_PASSWORD=$superuser_password"
  } >"$env_file"
  echo "wrote $env_file (keep it: it holds the database's password and the store's key)"
fi

[ $# -gt 0 ] || set -- up -d
exec docker compose -f "$root/deploy/docker-compose.yml" -f "$here/compose.yml" --env-file "$env_file" "$@"
