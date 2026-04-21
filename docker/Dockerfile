# syntax=docker/dockerfile:1
#
# Production image: multi-stage build producing a minimal runtime container.

# ── Stage 1: Build ───────────────────────────────────────────────────────────

FROM rust:1-bookworm AS builder
WORKDIR /src

# Cache dependency compilation: copy manifests first, build a dummy crate,
# then replace with real source so only application code changes bust cache.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release \
    && rm -rf src

COPY src/ src/
RUN touch src/main.rs src/lib.rs && cargo build --release

# ── Stage 2: Runtime ─────────────────────────────────────────────────────────

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        uidmap \
        fuse-overlayfs \
        util-linux \
        iproute2 \
    && rm -rf /var/lib/apt/lists/*

# Docker CLI only — needed for `docker inspect` / layer extraction at init.
# The daemon itself is not installed; mount the host socket at runtime.
RUN install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && . /etc/os-release \
    && ARCH="$(dpkg --print-architecture)" \
    && { \
        echo "Types: deb"; \
        echo "URIs: https://download.docker.com/linux/debian"; \
        echo "Suites: ${VERSION_CODENAME}"; \
        echo "Components: stable"; \
        echo "Architectures: ${ARCH}"; \
        echo "Signed-By: /etc/apt/keyrings/docker.asc"; \
    } > /etc/apt/sources.list.d/docker.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends docker-ce-cli \
    && rm -rf /var/lib/apt/lists/*

RUN echo "root:100000:65536" >> /etc/subuid \
    && echo "root:100000:65536" >> /etc/subgid

RUN test -x /usr/bin/newuidmap && test -x /usr/bin/newgidmap

COPY --from=builder /src/target/release/apiary /usr/local/bin/apiary
COPY docker/entrypoint.sh     /usr/local/bin/entrypoint.sh
COPY docker/verify-sandbox.sh /usr/local/bin/verify-sandbox.sh
RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/verify-sandbox.sh

# Default config: layers cached locally, overlays under /var/lib.
ENV APIARY_OVERLAY_DIR=/var/lib/apiary/overlays
ENV APIARY_LAYERS_DIR=/var/lib/apiary/layers
ENV APIARY_BIND=0.0.0.0:8080

EXPOSE 8080

ENTRYPOINT ["entrypoint.sh"]
# Container starts with an empty image registry. Clients populate it via
# `POST /api/v1/images` against the running daemon (see README, "Loading
# images at runtime").
CMD ["sh", "-c", "apiary init --layers-dir \"$APIARY_LAYERS_DIR\" --overlay-dir \"$APIARY_OVERLAY_DIR\" && exec apiary daemon --bind \"$APIARY_BIND\""]
