#!/bin/sh
# Writes the server's configuration to /tmp at start (nginx runs /docker-entrypoint.d/*.sh first,
# and default.conf includes it). The web app's address or addresses from FERRUM_PUBLIC_URL
# (space-separated: scheme and host, and the port if not the default) go into the pages'
# frame-ancestors and the relay's config. Anything else stops the start.
set -eu
origins=""
for u in ${FERRUM_PUBLIC_URL:-}; do
  u=${u%/}
  if ! printf '%s' "$u" | grep -Eq '^https?://[A-Za-z0-9.-]+(:[0-9]{1,5})?$'; then
    echo "editors: FERRUM_PUBLIC_URL holds '$u'; it must be the web app's address, such as https://erp.example.com" >&2
    exit 1
  fi
  origins="$origins $u"
done
origins=${origins# }
if [ -z "$origins" ]; then
  echo "editors: set FERRUM_PUBLIC_URL to the web app's address (https://erp.example.com)" >&2
  exit 1
fi
sed "s#@ORIGINS@#$origins#g" /etc/nginx/editors/server.conf.in > /tmp/editors.conf
sed "s#@ORIGINS@#$origins#g" /etc/nginx/editors/programs.conf.in > /tmp/editors-programs.conf
echo "editors: the relay may be framed by $origins"
