# syntax=docker/dockerfile:1

# ---- build stage ----
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY . .
# Build only the proxy binary in release mode. rustls is used for upstream TLS,
# so no OpenSSL system dependency is required.
RUN cargo build --release --bin suture

# ---- runtime stage ----
# distroless/cc provides glibc (needed by the dynamically linked binary) with no
# shell or package manager — small attack surface. ~20MB image.
FROM gcr.io/distroless/cc-debian12
COPY --from=builder /app/target/release/suture /usr/local/bin/suture

# Containers must listen on all interfaces (the in-code default is 127.0.0.1).
ENV SUTURE_LISTEN=0.0.0.0:8787 \
    SUTURE_OPENAI_BASE=https://api.openai.com \
    SUTURE_ANTHROPIC_BASE=https://api.anthropic.com

EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/suture"]
