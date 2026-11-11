#!/bin/sh
# Before the first `docker compose up`: what would stop this host from running Ferrum safely.
# Run from the repository: deploy/preflight.sh [--generate]
#   --generate  fills secrets missing from deploy/.env with new random ones, makes the online editor's WOPI
#               proof key (deploy/office-proof-key) when there is none, and changes nothing else
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
  # The online editor's WOPI proof key: with it, Ferrum refuses any WOPI call the editor did not sign.
  # The editor runs as another user and must read it, so the file is readable by all; it signs calls
  # to Ferrum and opens nothing by itself (a token is still needed).
  if [ ! -f office-proof-key ] && command -v openssl > /dev/null 2>&1; then
    openssl genrsa -traditional -out office-proof-key 2048 2> /dev/null && chmod 644 office-proof-key && echo "generated office-proof-key"
  fi
  if [ -f office-proof-key ] && [ -z "$(value OFFICE_PROOF_KEY)" ]; then
    echo "OFFICE_PROOF_KEY=./office-proof-key" >> .env
    echo "set OFFICE_PROOF_KEY"
  fi
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

# The online editor (Collabora, the `office` service) and the PDF renderer: on unless their address is set, empty.
office_set=$(grep -c '^FERRUM_OFFICE_URL=' .env 2>/dev/null || true)
if [ "$office_set" -gt 0 ] && [ -z "$(value FERRUM_OFFICE_URL)" ]; then
  ok "online editing is turned off (FERRUM_OFFICE_URL is empty)"
else
  office_url=$(value OFFICE_PUBLIC_URL)
  case "$(value PUBLIC_URL)" in
    "" | *localhost* | *127.0.0.1*) ;;
    *) [ -n "$office_url" ] || warning "OFFICE_PUBLIC_URL is not set" "Set it, and OFFICE_PUBLIC_HOST, to the editor's own address (https://office.example.com): browsers on other machines cannot reach localhost:9980." ;;
  esac
  case "$office_url" in
    "" | http://localhost* | http://127.0.0.1*) ;;
    http://*) warning "OFFICE_PUBLIC_URL is not https" "Serve the editor over TLS (and set OFFICE_TLS_TERMINATED=true): documents travel through it." ;;
  esac
  key=$(value OFFICE_PROOF_KEY)
  if [ -n "$key" ] && [ -f "$key" ]; then ok "the editor's WOPI proof key is set"
  else warning "no WOPI proof key for the online editor" "Run deploy/preflight.sh --generate: with a key, Ferrum refuses WOPI calls the editor did not sign."; fi
  # Reachable and offering what Drive opens, once it runs (this script usually runs before the first start).
  check_url=${office_url:-http://localhost:$(value OFFICE_PORT)}
  [ "$check_url" != "http://localhost:" ] || check_url=http://localhost:9980
  discovery=$(curl -fsS -m 5 "$check_url/hosting/discovery" 2> /dev/null || true)
  if [ -z "$discovery" ]; then
    warning "the online editor does not answer at $check_url" "Fine before the first start; after \`docker compose up -d\`, check \`docker compose logs office\`."
  else
    missing=""
    for ext in docx xlsx pptx; do grep -q "ext=\"$ext\" name=\"edit\"" <<< "$discovery" || missing="$missing $ext"; done
    if [ -z "$missing" ]; then ok "the online editor answers and opens docx, xlsx and pptx"
    else blocker "the online editor offers no editor for:$missing" "Check its image (COLLABORA_TAG) and \`docker compose logs office\`."; fi
  fi
fi

memory_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)
if [ "$memory_kb" -gt 0 ]; then
  if [ "$memory_kb" -lt 3900000 ]; then blocker "less than 4 GB of memory" "PostgreSQL's shared buffers alone are set to 512 MB; see docs/operations.md, Capacity."
  elif [ "$memory_kb" -lt 7800000 ]; then warning "less than 8 GB of memory" "Enough for a few people; see docs/operations.md, Capacity."
  else ok "memory: $((memory_kb / 1048576)) GB"; fi
  # The editor takes about 590 MB at rest and 50 to 60 MB for each small open document (docs/operations.md, Online editing).
  if [ "$memory_kb" -lt 5800000 ] && ! { [ "$office_set" -gt 0 ] && [ -z "$(value FERRUM_OFFICE_URL)" ]; }; then
    warning "less than 6 GB of memory with the online editor on" "Give the host more, or turn the editor off (FERRUM_OFFICE_URL= in deploy/.env)."
  fi
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
