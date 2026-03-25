# syntax=docker/dockerfile:1
# Сборка release внутри образа (musl), финальный слой — только Alpine + бинарник и статика.
FROM rust:1-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release \
    && strip target/release/ekza-rust-server

FROM alpine:3.21

RUN apk add --no-cache ca-certificates tini \
    && addgroup -g 1000 app \
    && adduser -D -H -u 1000 -G app app

WORKDIR /app

COPY --from=builder /app/target/release/ekza-rust-server /app/ekza-rust-server
COPY build/ /app/build/

RUN chown -R app:app /app && chmod 755 /app/ekza-rust-server

USER app

EXPOSE 3001

# Lock browser clients to our SPA origin (override with -e for previews / local dev).
ENV HOST=0.0.0.0 \
    PORT=3001 \
    LOG_LEVEL=info \
    STATIC_DIR=build \
    CORS_ALLOWED_ORIGINS=https://space.ekza.io

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["/app/ekza-rust-server"]
