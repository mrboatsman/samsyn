# Build context is the repository root, so the vendored third_party/yrs that
# server/Cargo.toml patches in (`../third_party/yrs`) is available.
#   docker build -f server/Dockerfile -t samsyn .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY server/Cargo.toml server/Cargo.lock server/
COPY server/src server/src
COPY third_party third_party
WORKDIR /src/server
# Version reported by /health and --version. Tagged builds pass the tag; unset falls
# back to Cargo.toml (which releases keep in step with manifest.json).
ARG SAMSYN_VERSION=
ENV SAMSYN_VERSION=$SAMSYN_VERSION
RUN cargo build --release --bin samsyn

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/server/target/release/samsyn /usr/local/bin/samsyn
ENV SAMSYN_DATA_DIR=/data SAMSYN_BIND=0.0.0.0:8787
VOLUME ["/data"]
EXPOSE 8787
EXPOSE 8788/udp
ENTRYPOINT ["samsyn"]
CMD ["serve"]
