# syntax=docker/dockerfile:1.7-labs
#
# The sandbox VM image: Debian with common toolchains, the stock `aho` CLI (TUI and rpc) and
# `cloud-guest` (the exec server and workspace file walker the runtime agent drives).
#
# Build with scripts/build-sandbox-image.sh: the context is the aho-cloud checkout, and the
# read-only aho checkout is the named context `aho-src` (aho-cloud's path dependencies need it too).

ARG RUST_IMAGE=rust:1-bookworm
ARG NODE_IMAGE=node:22-bookworm-slim
ARG BASE_IMAGE=debian:bookworm-slim

# Both Rust stages share one crates.io cache. It is `sharing=locked`: cargo's own lock lives in
# CARGO_HOME, outside the mount, so two stages unpacking the same crate at once would collide
# ("failed to unpack package … .cargo-ok: File exists").
FROM ${RUST_IMAGE} AS aho
WORKDIR /src/aho
COPY --from=aho-src --exclude=target --exclude=.git . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=aho-sandbox-cargo-registry,sharing=locked \
    --mount=type=cache,target=/cargo-target,id=aho-sandbox-aho \
    CARGO_TARGET_DIR=/cargo-target cargo build --release --locked --bin aho \
    && install -m 0755 /cargo-target/release/aho /usr/local/bin/aho

FROM ${RUST_IMAGE} AS guest
COPY --from=aho-src --exclude=target --exclude=.git . /src/aho
WORKDIR /src/aho-cloud
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=aho-sandbox-cargo-registry,sharing=locked \
    --mount=type=cache,target=/cargo-target,id=aho-sandbox-guest \
    CARGO_TARGET_DIR=/cargo-target cargo build --release --locked -p cloud-runtime --bin cloud-guest \
    && install -m 0755 /cargo-target/release/cloud-guest /usr/local/bin/cloud-guest

FROM ${NODE_IMAGE} AS node

FROM ${BASE_IMAGE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash-completion build-essential ca-certificates curl git jq less locales nano \
        openssh-client procps python3 python3-pip python3-venv ripgrep sudo unzip vim-tiny xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && sed -i 's/^# *en_US.UTF-8/en_US.UTF-8/' /etc/locale.gen && locale-gen
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && node --version && npm --version
COPY --from=aho /usr/local/bin/aho /usr/local/libexec/aho
COPY images/sandbox/aho-wrapper.sh /usr/local/bin/aho
COPY --from=guest /usr/local/bin/cloud-guest /usr/local/bin/cloud-guest

# The VM is the isolation boundary: the user may install packages, and aho runs its tools in its
# host world (inside this VM). The disk mounted at /workspace is the only state kept across
# restarts, so the home lives on it too: /home/aho -> /workspace/.aho/home (hidden from the
# console's Files tab, which skips `.aho`). Shell history, git config, caches, aho's config and
# `aho login` credentials survive a stop. `~/.aho` itself must stay a real directory: aho's
# credential store refuses a symlinked one. A new disk starts as a copy of this image's
# /workspace (Docker named-volume semantics; cloud-guest does the same on microVMs).
COPY images/sandbox/aho-config.toml /etc/aho/config.toml
COPY images/sandbox/motd.sh /etc/profile.d/aho-sandbox.sh
RUN useradd --create-home --uid 1000 --shell /bin/bash aho \
    && echo 'aho ALL=(ALL) NOPASSWD:ALL' >/etc/sudoers.d/aho && chmod 0440 /etc/sudoers.d/aho \
    && mkdir -p /workspace/.aho \
    && mv /home/aho /workspace/.aho/home \
    && ln -s /workspace/.aho/home /home/aho \
    && install -d -m 0700 /workspace/.aho/home/.aho \
    && cp /etc/aho/config.toml /workspace/.aho/home/.aho/config.toml \
    && chown -R aho:aho /workspace

ENV LANG=en_US.UTF-8 TERM=xterm-256color HOME=/home/aho
USER aho
WORKDIR /workspace
CMD ["sleep", "infinity"]
