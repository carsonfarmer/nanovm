# Minimal image for `nanovm serve` (default) and `nanovm proxy`.
# Build from the repository root:
#   docker build -f deploy/container/Dockerfile -t nanovm .
# Run flags (sysctl, fd limit, capabilities, init, volume): deploy/container/README.md.
# Base images are tags, not digests; pin them by digest for reproducible builds.

ARG RUST_IMAGE=docker.io/library/rust:1.99.1-slim-trixie
ARG RUNTIME_IMAGE=docker.io/library/debian:trixie-slim

FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/nanovm crates/nanovm
# A release build without the `dev` feature: dev mode cannot be configured.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p nanovm --bin nanovm \
    && install -D target/release/nanovm /out/nanovm

FROM ${RUNTIME_IMAGE}
# The node and the proxy refuse to start without any CA certificates
# (registries, the DNS-over-HTTPS resolver and egress use them).
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /out/nanovm /usr/local/bin/nanovm
COPY deploy/container/nanovm.toml deploy/container/proxy.toml /etc/nanovm/
# Object volumes, journals, the registry and compile caches. Root-owned, with
# per-app uids below it: the supervisor must run as root for those.
VOLUME ["/var/lib/nanovm"]
EXPOSE 8080 8443
ENTRYPOINT ["/usr/local/bin/nanovm"]
CMD ["serve", "--config", "/etc/nanovm/nanovm.toml"]
