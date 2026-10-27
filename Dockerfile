# syntax=docker/dockerfile:1
# Collector image. Build and run it with deploy/collector.sh rather than directly.

FROM rust:1.97-slim-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p st-collector \
    && cp target/release/collector /usr/local/bin/collector

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/collector /usr/local/bin/collector
ENV STOCKTRADER_DATA_DIR=/data/raw
ENTRYPOINT ["collector"]
CMD ["run"]
