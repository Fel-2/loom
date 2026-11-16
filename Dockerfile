# loom — two stages; the runtime needs git (loom shells out to it), nothing else.
FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data loom \
    && mkdir -p /data && chown loom /data && chmod 700 /data
COPY --from=build /src/target/release/loom /usr/local/bin/loom
USER loom
VOLUME /data
EXPOSE 3000 2222
ENV LOOM_DATA=/data \
    LOOM_ADDR=0.0.0.0:3000 \
    LOOM_SSH_ADDR=0.0.0.0:2222
HEALTHCHECK --interval=30s --timeout=3s CMD ["loom", "version"]
ENTRYPOINT ["loom"]
CMD ["serve"]
