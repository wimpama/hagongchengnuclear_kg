FROM qdrant/qdrant:v1.14.1

# The official image omits curl; use it for the real readiness health check.
USER 0:0
RUN apt-get update \
    && apt-get install --no-install-recommends -y curl \
    && rm -rf /var/lib/apt/lists/*
