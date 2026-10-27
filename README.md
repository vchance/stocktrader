# stocktrader

Automated US-equity trading driven by historical financials plus news and social-media signals, written in
Rust. The same strategy code runs in three modes: **backtest** (historical replay with simulated fills),
**paper** (live data, simulated or broker-paper fills) and **live** (real orders).

```
cargo test
cargo run -p st-cli -- backtest                    # synthetic data, no API keys needed
cargo run -p st-cli -- backtest --bars bars.csv    # symbol,date,open,high,low,close,volume
```

## Data collector

`collector` archives news and social data that **cannot be backfilled later**, verbatim, to
`data/raw/<source>/<YYYY-MM-DD>.jsonl` (one JSON object per HTTP response: `fetched_at`, `source`, `url`,
`status`, `body`). Parsing happens later from the archive, so improved parsers can be re-run over all history.

```
cp .env.example .env                                  # add whichever keys you have
cargo run --release -p st-collector -- sources        # which sources are enabled
cargo run --release -p st-collector -- once           # fetch everything once
cargo run --release -p st-collector -- run            # poll until Ctrl-C
```

| Source | Key | Every | Why it must be collected now |
|---|---|---|---|
| `apewisdom` | none | 15 min | Ticker mention counts; only the current snapshot is served |
| `reddit` | `REDDIT_CLIENT_ID/SECRET` | 2 min | New posts and comments; listings only reach back ~1000 items |
| `finnhub_news` | `FINNHUB_API_KEY` | 10 min | Free tier serves recent news only |
| `alpaca_news` | `APCA_API_KEY_ID/SECRET_KEY` | 5 min | Benzinga news with our own arrival timestamps (history is also backfillable) |

### Running on a server

On a Linux host with Docker (currently `jabrony`), from a checkout of `main`:

```
git clone https://github.com/vchance/stocktrader.git ~/stocktrader && cd ~/stocktrader
deploy/collector.sh up        # builds the image, creates .env (mode 600) and data/, starts with restart=unless-stopped
nano .env && deploy/collector.sh up    # add keys as plain KEY=value lines, then restart to pick them up
deploy/collector.sh status    # container state and which sources are enabled
deploy/collector.sh logs      # follow logs
deploy/collector.sh update    # git pull + rebuild + restart after a merge
```

The archive lands in `~/stocktrader/data/raw/`, owned by the deploying user.

SEC filings are deliberately not polled: EDGAR keeps a permanent archive with acceptance timestamps, so they
are a backfill job.

- **No silent gaps:** Reddit pages back to the newest item already archived; Alpaca resumes from a saved
  time window (up to 7 days of catch-up). Cursors live in `data/raw/.state/` and only advance after the
  records are synced to disk. A request that fails doesn't discard the rest of the poll.
- **Backoff** after failures is capped per source: none for Reddit, 2x Finnhub, 4x ApeWisdom, 16x Alpaca.
- **Readers** must dedupe (polls overlap on purpose) and skip unparseable lines (a crash can tear at most
  the last line of a file). File dates are UTC fetch dates, not exchange dates.
- Stop with Ctrl-C or SIGTERM. Check Reddit's and each provider's current API terms before relying on this.

## Decisions so far

| Topic | Decision |
|---|---|
| Language | Rust |
| Universe | All US equities, filtered by risk limits (min price $5, min 20-day avg dollar volume $5M by default) |
| Data budget | Free / cheap sources |
| Users | Single user, single account |
| Horizon | Undecided. The engine is bar-size agnostic; we start with daily bars (swing trading) because free daily data is plentiful and it avoids pattern-day-trader constraints |
| Broker | Alpaca first (paper and live share one API), IBKR later behind the same trait |

## Architecture

```
EventSource ──► Backtest / live runner ──► Strategy ──► Signal (target weight)
                     │                                       │
                     │                                 RiskManager (approve / clamp / reject)
                     │                                       │
                     └──── fills ◄──── ExecutionAdapter ◄────┘ sized OrderRequest
```

| Crate | Purpose |
|---|---|
| `st-core` | Domain types (`Bar`, `Event`, `Signal`, `OrderRequest`, `Fill`, `Portfolio`) and the `Clock`, `EventSource`, `Strategy`, `ExecutionAdapter` traits |
| `st-data` | Point-in-time replay (`VecEventSource`), CSV daily bars, synthetic data. Ingestion clients go here |
| `st-risk` | Pre-trade checks: position cap, gross exposure, daily loss halt, kill switch, price and liquidity filters |
| `st-engine` | Event-driven backtester, `SimulatedBroker` (fills at next bar open + slippage), metrics |
| `st-strategies` | Strategies. `SmaCrossover` is a plumbing test only |
| `st-cli` | `stocktrader` binary |

### Rules that keep backtests honest
- Every event carries `available_at`, the moment it became public. Bars are available at their close;
  fundamentals at their **filing** date, not period end. The backtester errors if a source goes back in time.
- Orders fill only against bars that opened after the order was submitted.
- Money is `rust_decimal::Decimal`, never `f64`.

## Planned free / cheap data sources (verify current limits and terms before relying on them)

| Need | Source |
|---|---|
| Daily bars, whole market | Alpaca Market Data (free plan, multi-symbol bar requests); Polygon/Massive free tier "grouped daily" (all tickers per call) |
| Universe and reference | Nasdaq Trader symbol directories, SEC `company_tickers.json`. Snapshot these daily: free delisted-ticker history is scarce |
| Fundamentals | SEC EDGAR XBRL `companyfacts` bulk download; each fact has a `filed` date for point-in-time use |
| Filings and insider trades | EDGAR 8-K and Form 4 feeds |
| News | Alpaca news API (Benzinga), Finnhub free company news |
| Social | Reddit API (free tier), aggregated mention counts (e.g. ApeWisdom), StockTwits where terms allow |
| Macro | FRED |

## Roadmap
1. Alpaca historical-bars client and a local store (Parquet / DuckDB); universe builder with liquidity filter.
2. ~~Start archiving news and social feeds~~ (`collector`, running on `jabrony`). Next: compress old files.
3. EDGAR fundamentals ingestion with point-in-time filing dates.
4. Text pipeline: ticker extraction, dedupe, sentiment scoring (FinBERT via ONNX, or an LLM), mention velocity.
5. Async live runner (`tokio`) with an Alpaca paper `ExecutionAdapter`. Before that: extract a mode-agnostic
   engine step from `Backtest` so paper/live don't duplicate the loop; decouple fills from market events
   (async fills, partials, rejects, cancels); reconcile positions and cash against the broker; give strategies
   fill and open-order feedback; persist kill-switch and session state across restarts; share one sizing function.
6. Dashboard, alerts, persistent kill switch; benchmark comparison (SPY) and walk-forward evaluation.
