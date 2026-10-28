FROM qdrant/qdrant:v1.14.1

# The official image omits curl; use it for the real readiness health check.
USER 0:0
ARG KB_APT_MIRROR=aliyun
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
    && apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 install --no-install-recommends -y curl \
    && rm -rf /var/lib/apt/lists/*
