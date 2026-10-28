# syntax=docker/dockerfile:1.7

# The knowledge service is built from the Rust service directory.  Keeping the
# build context small avoids copying the frontend and its node_modules into
# the image.
FROM rust:1.94-bookworm AS builder

# This is separate from the Docker image mirror used by BuildKit. Only the
# build stage uses it; select crates-io to use the original registry instead.
ARG KB_CARGO_REGISTRY=rsproxy
ARG TARGETPLATFORM
ENV CARGO_HTTP_MULTIPLEXING=false \
    CARGO_HTTP_TIMEOUT=120 \
    CARGO_NET_RETRY=5
RUN case "$KB_CARGO_REGISTRY" in \
      rsproxy) printf '%s\n' \
        '[source.crates-io]' 'replace-with = "rsproxy-sparse"' \
        '[source.rsproxy-sparse]' 'registry = "sparse+https://rsproxy.cn/index/"' \
        > /usr/local/cargo/config.toml ;; \
      crates-io) ;; \
      *) printf 'Unsupported KB_CARGO_REGISTRY: %s\n' "$KB_CARGO_REGISTRY" >&2; exit 1 ;; \
    esac

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY src ./src
COPY migrations ./migrations
COPY processing/pdf.py ./processing/pdf.py
COPY processing/ooxml.py ./processing/ooxml.py
COPY processing/spreadsheet.py ./processing/spreadsheet.py

# Registry downloads and compiler outputs survive a failed RUN or source edit
# on this builder. Fetch again even when reusing layers: cache mounts can be
# garbage-collected independently of successful image layers.
RUN --mount=type=cache,id=course-knowledge-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=course-knowledge-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=course-knowledge-target-rust-1.94-${TARGETPLATFORM},target=/build/target,sharing=locked \
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/^host: //p')" \
    && cargo build --release --locked --offline --jobs 1 --bin course-kb-server \
    && install -D /build/target/release/course-kb-server /out/course-kb-server

FROM node:22.19.0-bookworm-slim AS document-processing
WORKDIR /opt/knowledge-processing
COPY processing/package.json processing/package-lock.json ./
RUN --mount=type=cache,id=course-knowledge-npm,target=/root/.npm,sharing=locked \
    npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
      --fetch-timeout=120000 --fetch-retries=3
COPY processing/pptx.mjs ./

FROM debian:bookworm-slim AS runtime

ENV RUST_LOG=info \
    KB_BIND_ADDR=0.0.0.0:8080 \
    KB_DATA_DIR=/var/lib/knowledge-service/data \
    KB_CACHE_DIR=/var/lib/knowledge-service/data/cache \
    KB_PYTHON_BIN=/opt/knowledge-python/bin/python3 \
    KB_PPTX_PROCESSOR=/opt/knowledge-processing/pptx.mjs

ARG KB_APT_MIRROR=aliyun
# Slim images may not have CA certificates until this install. APT still
# verifies Debian's signed repository metadata and package checksums over HTTP.
RUN case "$KB_APT_MIRROR" in \
      aliyun) \
        for kb_apt_source in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do \
          if [ -f "$kb_apt_source" ]; then \
            sed -i \
              -e 's|https\?://deb.debian.org/|http://mirrors.aliyun.com/|g' \
              -e 's|https\?://security.debian.org/|http://mirrors.aliyun.com/|g' \
              "$kb_apt_source"; \
          fi; \
        done ;; \
      official) ;; \
      *) printf 'Unsupported KB_APT_MIRROR: %s\n' "$KB_APT_MIRROR" >&2; exit 1 ;; \
    esac \
    && apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 -o APT::Update::Error-Mode=any update \
    && apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 install --no-install-recommends -y ca-certificates curl libsqlite3-0 ffmpeg poppler-utils python3 python3-venv fonts-droid-fallback fonts-noto-cjk fonts-liberation \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 knowledge \
    && useradd --system --uid 10001 --gid 10001 --create-home --home-dir /var/lib/knowledge-service knowledge \
    && mkdir -p /var/lib/knowledge-service/data/cache \
    && chown -R knowledge:knowledge /var/lib/knowledge-service

COPY processing/requirements.txt /opt/knowledge-processing/requirements.txt
ARG KB_PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple
RUN --mount=type=cache,id=course-knowledge-pip,target=/root/.cache/pip,sharing=locked \
    python3 -m venv /opt/knowledge-python \
    && /opt/knowledge-python/bin/python3 -m pip install \
      --index-url "$KB_PIP_INDEX_URL" --timeout 60 --retries 3 --disable-pip-version-check \
      -r /opt/knowledge-processing/requirements.txt

COPY --from=builder /out/course-kb-server /usr/local/bin/course-kb-server
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
