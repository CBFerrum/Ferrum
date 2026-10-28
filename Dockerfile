# syntax=docker/dockerfile:1.7
# Ferrum API server with its packages. The web app ships separately (CBFerrum/Ferrum-Frontend).
#   docker build -t ferrum .
#   docker run -e DATABASE_URL=... -e JWT_SECRET=$(openssl rand -base64 48) -p 5150:5150 ferrum

FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
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
USER 10001
ENV LOCO_ENV=production \
    BINDING=0.0.0.0 \
    PORT=5150
EXPOSE 5150
ENTRYPOINT ["/usr/bin/tini", "--", "ferrum"]
CMD ["start"]
