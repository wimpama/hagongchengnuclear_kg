# syntax=docker/dockerfile:1.7

# The knowledge service is built from the Rust service directory.  Keeping the
# build context small avoids copying the frontend and its node_modules into
# the image.
FROM rust:1.94-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY src ./src
COPY migrations ./migrations
COPY processing/pdf.py ./processing/pdf.py
COPY processing/ooxml.py ./processing/ooxml.py
COPY processing/spreadsheet.py ./processing/spreadsheet.py

RUN cargo build --release --locked --jobs 1 --bin course-kb-server

FROM node:22.19.0-bookworm-slim AS document-processing
WORKDIR /opt/knowledge-processing
COPY processing/package.json processing/package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts
COPY processing/pptx.mjs ./

FROM debian:bookworm-slim AS runtime

ENV RUST_LOG=info \
    KB_BIND_ADDR=0.0.0.0:8080 \
    KB_DATA_DIR=/var/lib/knowledge-service/data \
    KB_CACHE_DIR=/var/lib/knowledge-service/data/cache \
    KB_PYTHON_BIN=/opt/knowledge-python/bin/python3 \
    KB_PPTX_PROCESSOR=/opt/knowledge-processing/pptx.mjs

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl libsqlite3-0 ffmpeg poppler-utils python3 python3-venv fonts-droid-fallback fonts-noto-cjk fonts-liberation \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 knowledge \
    && useradd --system --uid 10001 --gid 10001 --create-home --home-dir /var/lib/knowledge-service knowledge \
    && mkdir -p /var/lib/knowledge-service/data/cache \
    && chown -R knowledge:knowledge /var/lib/knowledge-service

COPY processing/requirements.txt /opt/knowledge-processing/requirements.txt
RUN python3 -m venv /opt/knowledge-python \
    && /opt/knowledge-python/bin/python3 -m pip install --no-cache-dir -r /opt/knowledge-processing/requirements.txt

COPY --from=builder /build/target/release/course-kb-server /usr/local/bin/course-kb-server
COPY --from=document-processing /usr/local/bin/node /usr/local/bin/node
COPY --from=document-processing /opt/knowledge-processing /opt/knowledge-processing

USER knowledge
WORKDIR /var/lib/knowledge-service

# Readiness checks both SQLite and Qdrant; /healthz remains the cheap process
# liveness endpoint for a separate supervisor.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:8080/readyz > /dev/null || exit 1

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/course-kb-server"]
