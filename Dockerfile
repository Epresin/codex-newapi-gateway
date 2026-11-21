FROM rust:slim-bookworm@sha256:2c3a22f0a5533ea2dd5a16627bc841228151faa2d4de2644ac9987e4a2f1f2fa AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/build/target cargo build --release --locked -j 2 && cp target/release/codex-newapi-gateway /gateway-bin
FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* && useradd -u 10001 -r gateway && mkdir -p /data/cache && chown -R gateway:gateway /data
COPY --from=build /gateway-bin /usr/local/bin/codex-newapi-gateway
USER gateway
EXPOSE 8080
ENTRYPOINT ["codex-newapi-gateway","--config","/etc/codex-gateway/config.toml"]
