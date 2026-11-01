#!/bin/sh
# Before the first `docker compose up`: what would stop this host from running Ferrum safely.
# Run from the repository: deploy/preflight.sh [--generate]
#   --generate  fills secrets missing from deploy/.env with new random ones, and changes nothing else
# Blockers exit non-zero; warnings do not. Nothing it prints is secret.
set -u
cd "$(dirname "$0")" || exit 1
blockers=0 warnings=0
blocker() { echo "BLOCKER  $1"; echo "         -> $2"; blockers=$((blockers + 1)); }
warning() { echo "warning  $1"; echo "         -> $2"; warnings=$((warnings + 1)); }
ok() { echo "ok       $1"; }

value() { sed -n "s/^$1=//p" .env 2>/dev/null | tail -1; }
placeholder() { case "$1" in "" | *[Cc][Hh][Aa][Nn][Gg][Ee]* | *example* | *replace*) return 0 ;; esac; return 1; }

if [ "${1:-}" = "--generate" ]; then
  [ -f .env ] || cp .env.example .env
  for pair in JWT_SECRET:48 FERRUM_SECRET_KEY:32 POSTGRES_PASSWORD:24 FERRUM_DB_PASSWORD:24 FERRUM_METRICS_TOKEN:32; do
    name=${pair%:*} bytes=${pair#*:}
    if placeholder "$(value "$name")"; then
      generated=$(openssl rand -base64 "$bytes" | tr -d '\n')
      grep -v "^$name=" .env > .env.new && echo "$name=$generated" >> .env.new && mv .env.new .env
      echo "generated $name"
    fi
  done
  chmod 600 .env
fi

command -v docker > /dev/null 2>&1 && ok "docker is installed" || blocker "docker is not installed" "Install Docker Engine."
docker compose version > /dev/null 2>&1 && ok "docker compose is available" || blocker "docker compose is not available" "Install the Compose plugin."

if [ ! -f .env ]; then
  blocker "deploy/.env does not exist" "Run deploy/preflight.sh --generate, then set PUBLIC_URL and MAILER_* in it."
else
  case "$(ls -l .env | cut -c5-10)" in
    ------) ok "deploy/.env is readable only by its owner" ;;
    *) warning "deploy/.env can be read by others" "chmod 600 deploy/.env" ;;
  esac
  for name in JWT_SECRET FERRUM_SECRET_KEY POSTGRES_PASSWORD FERRUM_DB_PASSWORD; do
    if placeholder "$(value "$name")"; then
      blocker "$name is missing, or a placeholder" "Run deploy/preflight.sh --generate."
    else
      ok "$name is set"
    fi
  done
  secret=$(value FERRUM_SECRET_KEY)
  if [ -n "$secret" ] && [ "$(printf %s "$secret" | openssl base64 -d -A 2>/dev/null | wc -c | tr -d ' ')" != 32 ]; then
    blocker "FERRUM_SECRET_KEY is not 32 bytes of base64" "openssl rand -base64 32"
  fi
  url=$(value PUBLIC_URL)
  case "$url" in
    https://*)
      ok "PUBLIC_URL is https"
      host=$(printf %s "$url" | sed -e 's#^https://##' -e 's#[:/].*##')
      if command -v getent > /dev/null 2>&1 && ! getent hosts "$host" > /dev/null 2>&1; then
        warning "$host does not resolve yet" "Point its DNS record at this host before people need it."
      fi
      ;;
    "") warning "PUBLIC_URL is not set" "Set it to the https address people open: links in mail and sign-in redirects use it." ;;
    *) warning "PUBLIC_URL is not https" "Put TLS in front of Ferrum: session cookies and passkeys need it." ;;
  esac
  [ -n "$(value FERRUM_METRICS_TOKEN)" ] && ok "metrics need a token" || warning "FERRUM_METRICS_TOKEN is not set" "Run deploy/preflight.sh --generate."
  [ -n "$(value MAILER_HOST)" ] && ok "a mail server is set" || warning "MAILER_HOST is not set" "Invitations and password resets will not go out."
  [ -n "$(value FERRUM_CLAMAV_ADDR)" ] && ok "a malware scanner is set" || warning "FERRUM_CLAMAV_ADDR is not set" "Uploads will not be scanned (docs/operations.md, Malware scanning)."
fi

memory_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)
if [ "$memory_kb" -gt 0 ]; then
  if [ "$memory_kb" -lt 3900000 ]; then blocker "less than 4 GB of memory" "PostgreSQL's shared buffers alone are set to 512 MB; see docs/operations.md, Capacity."
  elif [ "$memory_kb" -lt 7800000 ]; then warning "less than 8 GB of memory" "Enough for a few people; see docs/operations.md, Capacity."
  else ok "memory: $((memory_kb / 1048576)) GB"; fi
fi
free_kb=$(df -Pk . | awk 'NR == 2 {print $4}')
if [ "$free_kb" -lt 10485760 ]; then blocker "less than 10 GB free on this disk" "Images, the database and uploaded files live here."
else ok "disk: $((free_kb / 1048576)) GB free"; fi

for port in 80 443 5150; do
  if command -v ss > /dev/null 2>&1 && ss -ltn 2>/dev/null | grep -q ":$port "; then
    warning "something already listens on port $port" "Stop it, or change the port this stack publishes."
  fi
done

echo
echo "$blockers blockers, $warnings warnings"
[ "$blockers" -eq 0 ]
