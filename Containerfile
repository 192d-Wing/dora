# syntax=docker/dockerfile:1
#
# Multi-arch (linux/amd64, linux/arm64) build for dora.
# Build with buildx/podman, e.g.:
#   podman build -t dora .
#   docker buildx build --platform linux/amd64,linux/arm64 -f Containerfile -t dora .
#
# The runtime base is the Iron Bank hardened UBI 10 image, which requires
# authenticating to registry1.dso.mil first (see the CI `docker` job).

# ---- builder ----
# The Rust image is multi-arch, so under `buildx --platform` it builds natively
# for each target architecture.
FROM rust:1 AS builder
WORKDIR /usr/src/dora
COPY . .
# sqlx-cli creates the db + applies migrations so the sqlx query macros can run.
RUN cargo install sqlx-cli
RUN sqlx database create
RUN sqlx migrate run
ARG BUILD_MODE=release
RUN cargo build --${BUILD_MODE} --bin dora

# ---- runtime ----
# Iron Bank hardened UBI 10 (published for linux/amd64 and linux/arm64).
FROM registry1.dso.mil/ironbank/redhat/ubi/ubi10:10.2

# UBI is RHEL-based, so runtime deps come from dnf (not apt). The `dhcpd` user
# was previously provided by the isc-dhcp-server package on Ubuntu; UBI has no
# such package, so create the user/group directly. shadow-utils provides the
# useradd/groupadd/usermod/groupmod that useradd here and the entrypoint need;
# iproute provides `ip`, iputils provides `ping`.
RUN dnf -y install --setopt=install_weak_deps=False --nodocs \
        iproute \
        iputils \
        ca-certificates \
        shadow-utils \
    && dnf clean all \
    && rm -rf /var/cache/dnf \
    && groupadd -r dhcpd \
    && useradd -r -g dhcpd -s /sbin/nologin -d /var/lib/dora dhcpd

ARG BUILD_MODE=release
COPY --from=builder /usr/src/dora/target/${BUILD_MODE}/dora /usr/local/bin/dora

RUN mkdir -p /var/lib/dora/

COPY util/entrypoint.sh /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
