# syntax=docker/dockerfile:1
# Image with the collector and marketdata binaries. Build and run it with deploy/collector.sh.

FROM rust:1.97-slim-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p st-collector -p st-marketdata \
    && cp target/release/collector target/release/marketdata /usr/local/bin/

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/collector /usr/local/bin/marketdata /usr/local/bin/
ENV STOCKTRADER_DATA_DIR=/data/raw \
    STOCKTRADER_MARKETDATA_DIR=/data/marketdata
ENTRYPOINT ["collector"]
CMD ["run"]
