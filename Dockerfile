# Official Mergestro Gate image — the `slop-gate` CLI on Alpine, WITH the Rust
# toolchain + cargo-mutants baked in, so the behavioral mutation gate runs fully
# in-container (an Alpine base keeps a shell + apk, unlike distroless which has
# no toolchain and can't shell out to `cargo`).
#
# Multi-arch (amd64 + arm64): each architecture builds natively on its own runner
# (the mirror's docker.yml), not under emulation. musl static binary throughout.
#
# Size: the toolchain is most of the image, and most of it is needed — the gate
# compiles and tests every mutant. What mutants never use is deleted in its own
# stage and the result copied onto plain Alpine, because deleting files in a later
# layer saves nothing. At 0.6.1: 1.08 GB -> 804 MB, 359 -> 278 MB compressed, with
# identical gate results on a crate using a proc-macro, a build script and a doctest.

# ── Builder: the slop-gate and cargo-mutants binaries (musl, stripped). ─────
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
# The context is the OSS cut (the mirror's root, or `make docker-build`'s stage of it):
# Cargo.toml path-depends on the vendored turnover/crates/*, and a monorepo checkout of
# this dir has neither those crates nor a Cargo.lock (hence `Cargo.lock*`).
COPY Cargo.toml Cargo.lock* ./
COPY turnover ./turnover/
COPY src ./src/
RUN cargo build --release --bin slop-gate && strip target/release/slop-gate
# Built here, so its build caches never reach the runtime image.
RUN cargo install cargo-mutants --root /out && strip /out/bin/cargo-mutants

# ── Toolchain: rust:1-alpine's, minus what mutants never use. ───────────────
# None of these compile, link or test for the host musl target:
#  - rust-lld (~164 MB): rustc links the musl target through gcc;
#  - wasm-component-ld: WebAssembly only;
#  - the sanitizer runtimes (~74 MB): nightly-only `-Zsanitizer`;
#  - docs.
# rustdoc stays: `cargo test` runs doctests, and a mutant only a doctest catches
# must still be caught.
FROM rust:1-alpine AS toolchain
RUN T=/usr/local/rustup/toolchains/$(rustup toolchain list | head -1 | cut -d' ' -f1) \
 && B=$T/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p') \
 && rm -f $B/bin/rust-lld $B/bin/wasm-component-ld $B/lib/librustc-stable_rt.*.a \
 && rm -rf $T/share/doc $T/share/man /usr/local/rustup/downloads /usr/local/rustup/tmp

# ── Runtime: plain Alpine + the pruned toolchain + git + the two binaries. ──
FROM alpine:3.24 AS runtime
# gcc + musl-dev are what rust:1-alpine installs for rustc to link with; git is
# for the merge-base/diff.
RUN apk add --no-cache ca-certificates gcc musl-dev git
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
COPY --from=toolchain /usr/local/rustup /usr/local/rustup
COPY --from=toolchain /usr/local/cargo /usr/local/cargo
COPY --from=builder /out/bin/cargo-mutants /usr/local/bin/cargo-mutants
COPY --from=builder /app/target/release/slop-gate /usr/local/bin/slop-gate

# Run as a non-root user. /work is the mount point for the repo under test;
# cargo-mutants needs a writable target dir there + a writable TMPDIR, and cargo a
# writable CARGO_HOME for its registry cache. The two top directories only: a
# recursive chmod would copy every toolchain file into a new layer.
RUN adduser -D -u 65532 nonroot && chmod a+w /usr/local/cargo /usr/local/rustup
WORKDIR /work
ENV TMPDIR=/tmp
USER nonroot
ENTRYPOINT ["/usr/local/bin/slop-gate"]
