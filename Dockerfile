# syntax=docker/dockerfile:1
# Image with the collector, marketdata, fundamentals, signals and stocktrader binaries. Build and run it with
# deploy/collector.sh.

FROM rust:1.97-slim-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p st-collector -p st-marketdata -p st-fundamentals -p st-signals -p st-cli \
    && cp target/release/collector target/release/marketdata target/release/fundamentals \
          target/release/signals target/release/stocktrader /usr/local/bin/

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/collector /usr/local/bin/marketdata /usr/local/bin/fundamentals \
     /usr/local/bin/signals /usr/local/bin/stocktrader /usr/local/bin/
ENV STOCKTRADER_DATA_DIR=/data/raw \
    STOCKTRADER_MARKETDATA_DIR=/data/marketdata \
    STOCKTRADER_FUNDAMENTALS_DIR=/data/fundamentals \
    STOCKTRADER_SIGNALS_DIR=/data/signals \
    STOCKTRADER_LIVE_DIR=/data/live
ENTRYPOINT ["collector"]
CMD ["run"]
