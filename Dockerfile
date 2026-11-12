# syntax=docker/dockerfile:1.7
# Ferrum: the server with its packages and every page people open, including the shell's and Studio's
# interfaces, built from CBFerrum/Ferrum-Frontend at FRONTEND_REV (or another checkout of it:
# --build-context frontend=<dir>). Apps' own interfaces come compiled into their crates.
#   docker build -t ferrum .
#   docker run -e DATABASE_URL=... -e JWT_SECRET=$(openssl rand -base64 48) -p 8080:5150 ferrum

# The Ferrum-Frontend commit this server's pages are built from (bumped like an app's pin).
ARG FRONTEND_REV=6d4c321058c29eb7d233e9b549018476dd646cca

FROM scratch AS frontend
ARG FRONTEND_REV
ADD https://github.com/CBFerrum/Ferrum-Frontend.git#${FRONTEND_REV} /

FROM node:22-bookworm-slim@sha256:4b150b48b4116e2a22f2c39cc27025fbf4f153e2078fd3b14d159fdb5c7279da AS ui
WORKDIR /web
COPY --from=frontend package.json package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY --from=frontend . .
RUN npm run build:apps

FROM rust:1.95-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS build
# SAML's signatures are checked by xmlsec1 (crates/saml): its headers, and libclang to bind them.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libxml2-dev libxmlsec1-dev libclang-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
COPY third_party ./third_party
COPY server ./server
COPY packages ./packages
ARG BUILD_SHA=dev
ENV BUILD_SHA=${BUILD_SHA}
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p ferrum-server --features saml,process && cp target/release/ferrum /usr/local/bin/ferrum

FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini libxml2 libxmlsec1 libxmlsec1-openssl libssl3 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /app --shell /usr/sbin/nologin ferrum \
 && mkdir -p /var/lib/ferrum/files && chown -R 10001 /var/lib/ferrum
WORKDIR /app
COPY --from=build /usr/local/bin/ferrum /usr/local/bin/ferrum
COPY --from=build /src/server/config ./config
COPY --from=build /src/packages ./packages
# One directory per interface (shell, erp), registered at start-up (settings.ui_dir).
COPY --from=ui /web/dist-apps ./ui
USER 10001
ENV LOCO_ENV=production \
    BINDING=0.0.0.0 \
    PORT=5150 \
    FERRUM_UI_DIR=/app/ui
EXPOSE 5150
ENTRYPOINT ["/usr/bin/tini", "--", "ferrum"]
CMD ["start"]
