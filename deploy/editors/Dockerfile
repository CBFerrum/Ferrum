# The editor server for Drive's browser editors (docs/plan/browser-editors.md): the official,
# unmodified web builds of the editors in builds.txt, checked against their pinned SHA-256 sums,
# and Ferrum's relay, on unprivileged nginx. Static files only: no data, no secrets.
#
#   docker build -t ferrum-editors deploy/editors
#   docker run -p 8090:8080 -e FERRUM_PUBLIC_URL=https://erp.example.com ferrum-editors
FROM python:3.13-alpine AS site
WORKDIR /build
COPY builds.txt prepare.py ./
RUN python3 prepare.py /site /conf

FROM nginxinc/nginx-unprivileged:1.27-alpine
COPY --from=site /site /usr/share/nginx/html
COPY --from=site /conf/programs.conf.in /etc/nginx/editors/programs.conf.in
COPY server.conf.in headers.conf /etc/nginx/editors/
COPY default.conf /etc/nginx/conf.d/default.conf
COPY relay.html relay.js relay.css /usr/share/nginx/html/
COPY --chmod=0755 50-editors.sh /docker-entrypoint.d/50-editors.sh
