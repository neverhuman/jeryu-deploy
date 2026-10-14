# Minimal, locked-down sandbox image for a confined coding agent.
#
# Pairs with `OciSpec::from_agent_job` (crates/jeryu-runner-oci): that spec runs this
# image with a read-only root, all capabilities dropped, no-new-privileges, the
# seccomp profile below, a non-root user, memory/pid caps, `--network none`, and ONLY
# the agent's workspace bind-mounted at /workspace. So the image is the ONLY place the
# toolchain lives — there are no host mounts. It therefore carries exactly what a
# coding agent needs to build this repo and nothing else:
#   * the Rust toolchain (pinned), * Node + the Vite/TS/React toolchain,
#   * this repo's prefetched build dependencies (cargo + npm caches),
#   * the agent CLIs: Codex, Jekko, Claude,
#   * the pinned jankurai auditor (the in-sandbox CI lanes audit offline),
#   * the jeryu-git guard installed AS `git` (deny-by-default allowlist) + refusal wrappers
#     for gh/curl/wget/ssh/scp on a controlled PATH,
#   * a non-root user `agent` (uid/gid 1000).
# Credentials are NEVER baked in — they are injected per-run (see jeryu-agent-auth).
#
# Build:  docker build -f images/agent-sandbox/Dockerfile -t localhost/jeryu/agent-sandbox:latest .
#         (the runner references it via JERYU_AGENT_IMAGE)

# ---- toolchain build stage: compile the jeryu-git guard from the repo ----------------
# The guard lives in a Cargo WORKSPACE whose root Cargo.toml lists every member, so a
# manifest-path build of one crate still makes cargo read all members. We therefore copy
# the whole (already-small, see .dockerignore) context and build the one binary by
# package selector. The guard is pure std, so this is fast and self-contained.
FROM rust:1.95-slim-bookworm AS guard-build
ENV CARGO_HOME=/opt/rust/cargo
WORKDIR /src
COPY . /src
RUN cargo build --release -p jeryu-git-guard --bin jeryu-git \
    && cp target/release/jeryu-git /jeryu-git
# Warm this repo's cargo registry cache here, where the FULL workspace source exists, so
# the runtime image can ship it for an offline (`--network none`) first build. Doing it
# in this stage avoids copying every member crate into the runtime just to satisfy
# `cargo fetch`'s workspace-manifest read.
RUN cargo fetch --locked || cargo fetch || true

# ---- jankurai release stage: fetch the governed auditor (GENERATED — DO NOT EDIT) ----
# Rendered by jeryu-tool from tool-manifest.toml, the one pin of record. The
# auditor is a key-signed public Jankurai release: this stage downloads the
# pinned asset and its signed provenance, verifies both digests and both cosign
# signatures offline against the pinned release key with the pinned cosign,
# checks that the archive embeds that same provenance and that the provenance
# records the pinned source, lockfiles and toolchain, then checks the binary
# digest and version. The runtime container runs `--network none`, so the
# auditor ships in the image. This stage is digest-pinned to the runtime
# stage's Debian release (bookworm), so the version check below also proves
# the release binary's glibc requirement is met by the runtime.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS jankurai-build
ARG JANKURAI_RELEASE_REPO="https://github.com/neverhuman/jankurai-audit"
ARG JANKURAI_TAG="v1.7.5"
ARG JANKURAI_VERSION="jankurai 1.7.5"
ARG JANKURAI_SEMVER="1.7.5"
ARG JANKURAI_TARGET_TRIPLE="x86_64-unknown-linux-gnu"
ARG JANKURAI_ASSET="jankurai-1.7.5-x86_64-unknown-linux-gnu.tar.gz"
ARG JANKURAI_ASSET_SHA256="792c16465c274a266f64541152eb5c796fdf22113120133789d28bf99bcf915c"
ARG JANKURAI_BINARY_SHA256="d2414884a3dbe1cc2d02b4240588869f6afac26ff0f167829e907b1617501121"
ARG JANKURAI_PROVENANCE_ASSET="provenance-x86_64-unknown-linux-gnu.json"
ARG JANKURAI_PROVENANCE_SHA256="4da1f261f3ec0fa8a3dff3b920fbba29267ca7721a8c34baf5f7ad63a03fb8c8"
ARG JANKURAI_SOURCE_COMMIT="bb5a1ccaeda629ab2c36d99d20f9b89855ab6fcb"
ARG JANKURAI_SOURCE_TREE="9cd7195f548500505e7c7aa8b8260836448046ae"
ARG JANKURAI_FAMILY_LOCK_SHA256="076b4b7c03c1685589c3b860439eefe6bc70b7b7419b77052d87b74fc0e845f4"
ARG JANKURAI_CARGO_LOCK_SHA256="861176e141b043a70741ac8872ede9a2f2ca0b59c9f4ad24833f864076f47885"
ARG JANKURAI_RUSTC_VERSION="rustc 1.97.1 (8bab26f4f 2026-07-14)"
ARG JANKURAI_CARGO_VERSION="cargo 1.97.1 (c980f4866 2026-06-30)"
ARG JANKURAI_RELEASE_SIGNER="jankurai-release-2026.pub"
ARG JANKURAI_RELEASE_SIGNER_SHA256="99f48af11e4e6463544652ddfaefb0e9953de15fb0802b174123ac2907ff06fc"
ARG JANKURAI_COSIGN_VERSION="v3.1.3"
ARG JANKURAI_COSIGN_SHA256="4629c757b7618056f8ddd7e2625ae9fdd94c0372a65049520bc7d9df9efc7f71"
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    work="$(mktemp -d)"; cd "$work"; \
    fail() { echo "jankurai release stage: $*" >&2; exit 1; }; \
    fetch() { curl --proto '=https' --tlsv1.2 -fsSL --retry 3 "$1" -o "$2"; }; \
    fetch "https://github.com/sigstore/cosign/releases/download/${JANKURAI_COSIGN_VERSION}/cosign-linux-amd64" cosign; \
    echo "${JANKURAI_COSIGN_SHA256}  cosign" | sha256sum -c -; \
    chmod 0755 cosign; \
    base="${JANKURAI_RELEASE_REPO}/releases/download/${JANKURAI_TAG}"; \
    fetch "$base/${JANKURAI_RELEASE_SIGNER}" release.pub; \
    fetch "$base/${JANKURAI_ASSET}" "${JANKURAI_ASSET}"; \
    fetch "$base/${JANKURAI_ASSET}.cosign.bundle" "${JANKURAI_ASSET}.cosign.bundle"; \
    fetch "$base/${JANKURAI_PROVENANCE_ASSET}" "${JANKURAI_PROVENANCE_ASSET}"; \
    fetch "$base/${JANKURAI_PROVENANCE_ASSET}.cosign.bundle" "${JANKURAI_PROVENANCE_ASSET}.cosign.bundle"; \
    echo "${JANKURAI_RELEASE_SIGNER_SHA256}  release.pub" | sha256sum -c -; \
    echo "${JANKURAI_ASSET_SHA256}  ${JANKURAI_ASSET}" | sha256sum -c -; \
    echo "${JANKURAI_PROVENANCE_SHA256}  ${JANKURAI_PROVENANCE_ASSET}" | sha256sum -c -; \
    ./cosign verify-blob "${JANKURAI_ASSET}" --bundle "${JANKURAI_ASSET}.cosign.bundle" \
      --key release.pub --insecure-ignore-tlog=true --offline=true; \
    ./cosign verify-blob "${JANKURAI_PROVENANCE_ASSET}" --bundle "${JANKURAI_PROVENANCE_ASSET}.cosign.bundle" \
      --key release.pub --insecure-ignore-tlog=true --offline=true; \
    stem="${JANKURAI_ASSET%.tar.gz}"; \
    tar -tzf "${JANKURAI_ASSET}" | sed 's:/$::' | LC_ALL=C sort > inventory; \
    printf '%s\n' "$stem" "$stem/jankurai" "$stem/family.lock" "$stem/Cargo.lock" \
      "$stem/LICENSE" "$stem/provenance.json" | LC_ALL=C sort > expected-inventory; \
    cmp -s inventory expected-inventory || fail "unexpected release archive inventory"; \
    if tar -tvzf "${JANKURAI_ASSET}" | grep -qv '^[-d]'; then fail "unsafe release archive entry"; fi; \
    mkdir payload; \
    tar -xzf "${JANKURAI_ASSET}" --no-same-owner --no-same-permissions -C payload; \
    cmp -s "payload/$stem/provenance.json" "${JANKURAI_PROVENANCE_ASSET}" || \
      fail "archive provenance differs from the signed release provenance"; \
    echo "${JANKURAI_FAMILY_LOCK_SHA256}  payload/$stem/family.lock" | sha256sum -c -; \
    echo "${JANKURAI_CARGO_LOCK_SHA256}  payload/$stem/Cargo.lock" | sha256sum -c -; \
    jq -e \
      --arg repo "${JANKURAI_RELEASE_REPO}" --arg tag "${JANKURAI_TAG}" \
      --arg version "${JANKURAI_SEMVER}" --arg target "${JANKURAI_TARGET_TRIPLE}" \
      --arg commit "${JANKURAI_SOURCE_COMMIT}" --arg tree "${JANKURAI_SOURCE_TREE}" \
      --arg family "${JANKURAI_FAMILY_LOCK_SHA256}" --arg cargo_lock "${JANKURAI_CARGO_LOCK_SHA256}" \
      --arg rustc "${JANKURAI_RUSTC_VERSION}" --arg cargo "${JANKURAI_CARGO_VERSION}" \
      '.schema == "jankurai.release/v2" and .repository == $repo and .tag == $tag and \
       .version == $version and .target == $target and \
       .commit == $commit and .tree == $tree and \
       .family_lock_sha256 == $family and .cargo_lock_sha256 == $cargo_lock and \
       (.toolchain.rustc | split("\n") | .[0]) == $rustc and .toolchain.cargo == $cargo and \
       .signing == "cosign-key"' \
      "${JANKURAI_PROVENANCE_ASSET}" >/dev/null || \
      fail "release provenance disagrees with the pin"; \
    install -D -m 0755 "payload/$stem/jankurai" /opt/rust/cargo/bin/jankurai; \
    echo "${JANKURAI_BINARY_SHA256}  /opt/rust/cargo/bin/jankurai" | sha256sum -c -; \
    /opt/rust/cargo/bin/jankurai --version | grep -qx "${JANKURAI_VERSION}" || \
      fail "release binary does not run on this base or reports another version"; \
    cd /; rm -rf "$work"

# ---- runtime image -------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

ARG RUST_VERSION=1.95.0
ARG NODE_MAJOR=22
ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/opt/rust/cargo \
    RUSTUP_HOME=/opt/rust/rustup \
    PATH=/opt/jeryu/bin:/opt/rust/cargo/bin:/usr/local/bin:/usr/bin:/bin \
    JERYU_REAL_GIT=/usr/bin/git

# Base packages: ONLY what a Rust/Node build of this repo needs. No ssh, no docker CLI.
# `passwd` provides groupadd/useradd (the agent user is created below); it is not in the
# slim base. The rest is exactly what a Rust/Node build of this repo needs.
RUN apt-get update && apt-get install -y --no-install-recommends \
      git ca-certificates curl build-essential pkg-config libssl-dev \
      jq python3 passwd \
    && rm -rf /var/lib/apt/lists/*

# Rust toolchain (pinned), no docs/components beyond the build set.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "${RUST_VERSION}" \
    && /opt/rust/cargo/bin/rustup component add clippy rustfmt

# The governed jankurai auditor (KEEP IN SYNC with ops/ci/lib.sh). The runtime
# is `--network none`, so the auditor must ship in the image. It lands at the exact path
# is given explicit API and shell-verifier paths, so no lane uses a bare PATH
# lookup that a stale build could shadow.
ARG JANKURAI_VERSION="jankurai 1.7.5"
ARG JANKURAI_BINARY_SHA256="d2414884a3dbe1cc2d02b4240588869f6afac26ff0f167829e907b1617501121"
COPY --from=jankurai-build /opt/rust/cargo/bin/jankurai /opt/rust/cargo/bin/jankurai
COPY images/agent-sandbox/jankurai-installation-receipt.json \
  /opt/jeryu/receipts/jankurai/sha256/cdfb59320d2ff9e1cfa731d25de4b6bee44fe94854eb217e415159df1826b461.json
COPY ops/ci/ensure-jankurai.sh /opt/jeryu/governance/ensure-jankurai.sh
ENV JERYU_JANKURAI_BIN=/opt/rust/cargo/bin/jankurai \
    JERYU_GOVERNED_JANKURAI_BIN=/opt/rust/cargo/bin/jankurai \
    JERYU_JANKURAI_RECEIPT=/opt/jeryu/receipts/jankurai/sha256/cdfb59320d2ff9e1cfa731d25de4b6bee44fe94854eb217e415159df1826b461.json
RUN test ! -L /opt/rust/cargo/bin/jankurai \
    && test "$(stat -c '%h' /opt/rust/cargo/bin/jankurai)" = "1" \
    && test "$(sha256sum /opt/rust/cargo/bin/jankurai | awk '{print $1}')" = "${JANKURAI_BINARY_SHA256}" \
    && /opt/rust/cargo/bin/jankurai --version | grep -qx "${JANKURAI_VERSION}" \
    && chmod 0444 "${JERYU_JANKURAI_RECEIPT}" \
    && chmod 0555 /opt/jeryu/governance/ensure-jankurai.sh \
    && bash /opt/jeryu/governance/ensure-jankurai.sh

# Node + the Vite/TS/React toolchain, then the agent CLIs.
RUN curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && npm install -g vite typescript \
       @openai/codex @anthropic-ai/claude-code \
    && rm -rf /var/lib/apt/lists/* /root/.npm

# Jekko (jeryu's own agent CLI) — installed from the internal registry/release.
# Kept as a clearly-marked install hook so the image build stays reproducible.
RUN npm install -g @jeryu/jekko-cli || echo "jekko: install from internal registry at build time"

# The git guard becomes the ONLY `git` on the agent PATH; a refusal wrapper is installed
# as every tool a "just edit + build + commit code in this branch" agent does NOT need.
# The agent reads/edits files and runs the build (cargo/rustc/rustup/node/npm/npx/tsc/
# vite) and commits via the git guard — nothing else. Wrapped tools fall into:
#   * networking (jeryu mediates all network + git):
#       gh curl wget ssh scp sftp nc ncat netcat telnet socat
#       git-remote-https git-remote-http
#   * symlinks (escape the workspace / link to host paths — never needed here): ln
#   * privilege escalation: sudo su doas
#   * package installs (the image is fixed; deps are prefetched):
#       apt apt-get aptitude dpkg pip pip3 gem
#   * container / orchestration: docker podman nerdctl kubectl ctr runc
#   * scheduling / daemons: crontab at batch systemctl service
#   * mount / disk / namespace escape: mount umount dd mkfs nsenter unshare chroot setarch
#   * keys / ownership / external-open: ssh-keygen ssh-add gpg chown xdg-open open
COPY --from=guard-build /jeryu-git /opt/jeryu/bin/git
COPY images/agent-sandbox/bin/refuse /opt/jeryu/bin/refuse
RUN for tool in \
      gh curl wget ssh scp sftp nc ncat netcat telnet socat \
      git-remote-https git-remote-http \
      ln \
      sudo su doas \
      apt apt-get aptitude dpkg pip pip3 gem \
      docker podman nerdctl kubectl ctr runc \
      crontab at batch systemctl service \
      mount umount dd mkfs nsenter unshare chroot setarch \
      ssh-keygen ssh-add gpg chown xdg-open open ; do \
      cp /opt/jeryu/bin/refuse "/opt/jeryu/bin/${tool}"; done \
    && chmod -R 0555 /opt/jeryu/bin

# The seccomp profile the runner references at /opt/jeryu/seccomp/<name>.json.
COPY images/agent-sandbox/seccomp/ /opt/jeryu/seccomp/

# Prefetch this repo's build dependencies so the agent's first build is warm and the
# container needs no network (it runs --network none). The cargo registry cache is taken
# from the guard-build stage (which had the FULL workspace source, so `cargo fetch` could
# resolve every member); the npm deps are installed here from the lockfile.
WORKDIR /opt/jeryu/repo
COPY --from=guard-build /opt/rust/cargo/registry /opt/rust/cargo/registry
COPY Cargo.toml Cargo.lock ./
COPY apps/web/package.json apps/web/package-lock.json ./apps/web/
RUN (cd apps/web && npm ci --no-audit --no-fund || true)

# Non-root user (uid/gid 1000). The container runs `--read-only`, so the ONLY writable
# spots are `--tmpfs /tmp` and the workspace bind-mount. The agent CLIs (codex/claude)
# write under $HOME (~/.codex, ~/.claude, ~/.cache); pointing HOME at a subdir of the
# writable workspace mount lets them start without touching the read-only root. The
# entrypoint creates $HOME on first use (a fresh workspace mount has no such dir yet),
# then execs the agent argv emitted by the runner (codex / claude / jekko ...).
COPY images/agent-sandbox/bin/entrypoint /opt/jeryu/bin/entrypoint
RUN chmod 0555 /opt/jeryu/bin/entrypoint
# groupadd/useradd live in /usr/sbin, which is intentionally NOT on the agent PATH above;
# call them by absolute path so the build does not depend on PATH.
RUN /usr/sbin/groupadd -g 1000 agent \
    && /usr/sbin/useradd -u 1000 -g 1000 -m -s /bin/bash agent \
    && mkdir -p /workspace && /bin/chown agent:agent /workspace
USER agent
WORKDIR /workspace
ENV HOME=/workspace/.agent-home TERM=xterm-256color JERYU_REAL_GIT=/usr/bin/git
ENTRYPOINT ["/opt/jeryu/bin/entrypoint"]
