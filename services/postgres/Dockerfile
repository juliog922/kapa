ARG POSTGRES_VERSION=18
FROM postgres:${POSTGRES_VERSION}-alpine

LABEL org.opencontainers.image.title="kapa-postgres" \
      org.opencontainers.image.licenses="MIT"

# Scripts here run once, only when the data volume is initialised for the first time.
COPY initdb/ /docker-entrypoint-initdb.d/