# syntax=docker/dockerfile:1

FROM rust:1-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        uidmap \
        fuse-overlayfs \
        util-linux \
        iproute2 \
        procps \
        gosu \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash apiary \
    && echo "apiary:100000:65536" >> /etc/subuid \
    && echo "apiary:100000:65536" >> /etc/subgid

RUN test -x /usr/bin/newuidmap && test -x /usr/bin/newgidmap

COPY docker/entrypoint.sh     /usr/local/bin/entrypoint.sh
COPY docker/verify-sandbox.sh /usr/local/bin/verify-sandbox.sh
RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/verify-sandbox.sh

ENV APIARY_USER=apiary
WORKDIR /workspace

ENTRYPOINT ["entrypoint.sh"]
CMD ["bash"]
