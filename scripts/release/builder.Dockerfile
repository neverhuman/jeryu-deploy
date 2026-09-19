# The jeryu release builder: Ubuntu 22.04 (glibc 2.35, the forge host's) with a
# pinned Rust toolchain. stage-release.sh builds it on the build host when its
# tag is missing, so a release never depends on an image another project owns.
# Changing anything here needs a new tag in stage-release.sh (the `-rN` suffix).
FROM ubuntu:22.04

ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
        ca-certificates curl build-essential pkg-config git \
    && rm -rf /var/lib/apt/lists/* \
    && ldd --version | head -1 | grep -F "2.35"

ARG RUSTUP_VERSION=1.29.1
ARG RUSTUP_INIT_SHA256=dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71
ARG RUST_TOOLCHAIN=1.95.0
ENV RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo RUSTUP_TOOLCHAIN=${RUST_TOOLCHAIN}
ENV PATH=/opt/rust/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN set -eu; \
    curl --proto '=https' --tlsv1.2 -fsSL -o /tmp/rustup-init \
        "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init"; \
    echo "${RUSTUP_INIT_SHA256}  /tmp/rustup-init" | sha256sum -c -; \
    chmod +x /tmp/rustup-init; \
    /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "${RUST_TOOLCHAIN}"; \
    rm -f /tmp/rustup-init; \
    rustc -vV | grep -F "release: ${RUST_TOOLCHAIN}"; \
    test -x "/opt/rust/rustup/toolchains/${RUST_TOOLCHAIN}-x86_64-unknown-linux-gnu/bin/cargo"; \
    chmod -R a+rX /opt/rust

WORKDIR /work
CMD ["bash"]
