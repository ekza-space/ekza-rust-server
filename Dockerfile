FROM alpine:3.21

WORKDIR /app

RUN apk add --no-cache ca-certificates tini \
    && addgroup -g 1000 app \
    && adduser -D -H -u 1000 -G app app

COPY dist/server-linux-amd64 /app/server
COPY build/ /app/build/

RUN chown -R app:app /app \
    && chmod 755 /app/server

USER app

EXPOSE 3001

ENV HOST=0.0.0.0 \
    PORT=3001 \
    LOG_LEVEL=info \
    STATIC_DIR=build

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD wget -qO- http://127.0.0.1:${PORT}/health >/dev/null || exit 1

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["/app/server"]
