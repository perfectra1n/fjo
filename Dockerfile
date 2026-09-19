# The image is assembled from the musl artifacts release.yaml has already built
# and published, not compiled here. That is the point: the bytes in the image are
# byte-identical to the bytes someone downloads from the release page, so there is
# no second toolchain that can drift from the first.
#
# Base choice is forced, not preferred. `scratch` and distroless/static both fail:
#
#   - reqwest is built with rustls-native-certs, which reads the OS trust store at
#     runtime. An image with no /etc/ssl/certs cannot complete a single TLS
#     handshake, so ca-certificates is mandatory.
#   - forgejo-core/src/context/git/cli.rs shells out to `git` to resolve the host
#     and repository from the checkout. Without git on PATH, every repo-context
#     command (which is most of them) loses its default and needs an explicit
#     -R/--host.
#
# alpine + those two packages is the smallest base that actually works, and it
# pairs with the musl build we already produce.
FROM alpine:3.21

RUN apk add --no-cache ca-certificates git

# git refuses to operate on a repository owned by a different uid ("detected
# dubious ownership"). In a container that is the normal case -- the host checkout
# is bind-mounted and owned by whoever ran docker -- and the failure surfaces as
# fjo silently losing its repo context rather than as a git error the user sees.
RUN git config --system --add safe.directory '*'

ARG TARGETARCH
COPY dist/${TARGETARCH}/fjo /usr/local/bin/fjo

# Where a CI job is expected to mount its checkout.
WORKDIR /workspace

LABEL org.opencontainers.image.source="https://github.com/perfectra1n/fjo" \
      org.opencontainers.image.description="fjo - a command-line interface for Forgejo" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

ENTRYPOINT ["/usr/local/bin/fjo"]
