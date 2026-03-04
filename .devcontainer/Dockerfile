# syntax=docker/dockerfile:1

ARG VARIANT="1-1-bookworm"
FROM mcr.microsoft.com/devcontainers/rust:${VARIANT}

RUN apt-get update \
    && export DEBIAN_FRONTEND=noninteractive \
    && apt-get install -y --no-install-recommends \
        uidmap \
        fuse-overlayfs \
        util-linux \
        iproute2 \
        procps \
    && rm -rf /var/lib/apt/lists/*

# Configure subordinate ID ranges used by newuidmap/newgidmap.
RUN set -eux; \
    for user in root vscode; do \
        if id -u "${user}" >/dev/null 2>&1; then \
            grep -q "^${user}:" /etc/subuid || echo "${user}:100000:65536" >> /etc/subuid; \
            grep -q "^${user}:" /etc/subgid || echo "${user}:100000:65536" >> /etc/subgid; \
        fi; \
    done; \
    test -x /usr/bin/newuidmap; \
    test -x /usr/bin/newgidmap

USER vscode
