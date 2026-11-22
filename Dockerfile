# dither as a container. Packages a binary built outside it: CI builds the
# static musl binary per architecture (the `linux-build` job) and builds this
# with dist/ as the context, holding <arch>/dither. By hand:
# `docker build -f Dockerfile dist`.

FROM scratch

ARG TARGETARCH

# rustls' platform verifier needs a system trust store on Linux, and does not
# fall back to compiled-in roots: without this, signing in (the identity
# provider's token endpoint) and a bucket both fail.
COPY --from=gcr.io/distroless/static:latest@sha256:15f8637509792e05b97d83451b0b6ccfa4abd548f997e406ac1e72c87fe1fa78 \
     /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

COPY ${TARGETARCH}/dither /dither

EXPOSE 8080 9464
ENV BIND_ADDR=0.0.0.0:8080
ENV STATE_DIR=/data/state
ENV STORAGE=file:///data/objects

USER 65532:65532

LABEL org.opencontainers.image.title="dither"
LABEL org.opencontainers.image.licenses="MIT"
LABEL org.opencontainers.image.source="https://github.com/radiosilence/dither"

ENTRYPOINT ["/dither"]
