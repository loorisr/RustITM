# syntax=docker/dockerfile:1.7
# Static (musl) binary built natively on Alpine, shipped in an empty "scratch" image.
# Multi-arch: docker buildx build --platform linux/amd64,linux/arm64 .

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p itm-server \
 && cp target/release/itm-server /itm-server \
 && mkdir -p /data/dem-cache

FROM scratch
LABEL org.opencontainers.image.title="itm-api" \
      org.opencontainers.image.description="ITM (Longley-Rice) radio propagation API + web UI, Copernicus GLO-90 DEM" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
COPY --from=build /itm-server /itm-server
COPY --from=build --chown=10001:10001 /data /data
USER 10001:10001
ENV ITM_BIND=0.0.0.0:8080 \
    DEM_CACHE_DIR=/data/dem-cache \
    DEM_DOWNLOAD=true \
    RUST_LOG=info
VOLUME ["/data/dem-cache"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 CMD ["/itm-server", "healthcheck"]
ENTRYPOINT ["/itm-server"]
CMD ["serve"]
