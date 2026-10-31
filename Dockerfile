# syntax=docker/dockerfile:1.7
# Ferrum: the server with its packages and every page people open, including the shell's and ERP's
# interfaces, built from CBFerrum/Ferrum-Frontend at FRONTEND_REV (or another checkout of it:
# --build-context frontend=<dir>). Apps' own interfaces come compiled into their crates.
#   docker build -t ferrum .
#   docker run -e DATABASE_URL=... -e JWT_SECRET=$(openssl rand -base64 48) -p 8080:5150 ferrum

# The Ferrum-Frontend commit this server's pages are built from (bumped like an app's pin).
ARG FRONTEND_REV=8fc39221d38d4c94cc5959188eb6595bdd101def

FROM scratch AS frontend
ARG FRONTEND_REV
ADD https://github.com/CBFerrum/Ferrum-Frontend.git#${FRONTEND_REV} /

FROM node:22-bookworm-slim AS ui
WORKDIR /web
COPY --from=frontend package.json package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY --from=frontend . .
RUN npm run build:apps

FROM rust:1.95-bookworm AS build
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
    cargo build --release --locked -p ferrum-server && cp target/release/ferrum /usr/local/bin/ferrum

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
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
