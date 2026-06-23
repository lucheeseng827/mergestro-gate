# Official Mergestro Gate image — the `slop-gate` CLI on Alpine, WITH the Rust
# toolchain + cargo-mutants baked in, so the behavioral mutation gate runs fully
# in-container (an Alpine base keeps a shell + apk, unlike distroless which has
# no toolchain and can't shell out to `cargo`).
#
# Multi-arch (amd64 + arm64): buildx compiles natively in each target via QEMU,
# so no cross-toolchain plumbing is needed. musl static binary throughout.

# ── Builder: compile the slop-gate binary (musl). ───────────────────────────
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
# The crate is standalone here (its own [workspace] root). target/ is kept out
# by .dockerignore.
COPY Cargo.toml Cargo.lock ./
COPY src ./src/
RUN cargo build --release --bin slop-gate

# ── Runtime: Alpine + Rust toolchain + cargo-mutants. ───────────────────────
# rust:1-alpine already carries cargo + rustc (the gate shells out to them to
# compile + test each mutant). Add git (merge-base/diff) and cargo-mutants (the
# Rust mutation engine), then drop the install caches to keep the layer lean.
FROM rust:1-alpine AS runtime
RUN apk add --no-cache git ca-certificates \
    && cargo install cargo-mutants --root /usr/local \
    && rm -rf /usr/local/cargo/registry /usr/local/cargo/git ~/.cargo/registry ~/.cargo/git

COPY --from=builder /app/target/release/slop-gate /usr/local/bin/slop-gate

# Run as a non-root user. /work is the mount point for the repo under test;
# cargo-mutants needs a writable target dir there + a writable TMPDIR.
RUN adduser -D -u 65532 nonroot
WORKDIR /work
ENV TMPDIR=/tmp
USER nonroot
ENTRYPOINT ["/usr/local/bin/slop-gate"]
