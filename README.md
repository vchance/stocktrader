# stocktrader

Automated US-equity trading driven by historical financials plus news and social-media signals, written in
Rust. The same strategy code runs in three modes: **backtest** (historical replay with simulated fills),
**paper** (live data, simulated or broker-paper fills) and **live** (real orders).

```
cargo test
cargo run -p st-cli -- backtest                    # synthetic data, no API keys needed
cargo run -p st-cli -- backtest --bars bars.csv    # symbol,date,open,high,low,close,volume
cargo run --release -p st-cli -- backtest --parquet data/marketdata/bars_daily_all.parquet \
    --start 2022-01-01 --symbols AAPL,MSFT,NVDA         # real data from `marketdata sync`
cargo run --release -p st-cli -- walk-forward --parquet data/marketdata/bars_daily_all.parquet \
    --start 2018-01-01 --symbols AAPL,MSFT,NVDA         # tune on rolling windows, report only unseen ones
```

## Evaluating a strategy honestly

- **Benchmark:** every run is compared with buy-and-hold of `--benchmark` (default SPY, read from the
  same data even if `--symbols` omits it; `none` to skip): its metrics, the strategy's excess CAGR, and
  the correlation of daily returns.
- **Walk-forward:** `walk-forward` picks parameters from a small grid on each training window
  (`--train-days`, default 365) by Sharpe, then runs the choice on the next `--test-days` (91) only,
  after replaying `--warmup-days` (330, enough for a 200-day average plus the liquidity window; too little
  is refused, since a candidate that can't warm up never trades and would win on a Sharpe of 0) of
  history without trading so indicators are ready. The
  compounded test windows are the number to believe; the table also shows each choice's in-sample return,
  and the gap between the two is the overfitting. A final window cut short by the end of the data is
  dropped.
- **Price filter on traded prices:** `--min-price` (default $5) is applied to as-traded closes, read
  automatically from `bars_daily_raw.parquet` beside `--parquet` (or `--raw`). Adjusted history
  understates pre-split prices, so filtering on it would wrongly drop stocks that later split; the CLI
  warns if no raw file is found.

## Market data

`marketdata sync` (needs Alpaca keys) writes to `data/marketdata/`:

| File | Contents |
|---|---|
| `assets/<date>-active.json`, `-inactive.json` | Raw Alpaca asset lists, kept daily: our record of listings and delistings |
| `bars_daily_all.parquet` | Daily bars since 2016, split- and dividend-adjusted (use for returns) |
| `bars_daily_raw.parquet` | The same bars as traded (use for price filters such as "above $5") |

The universe is every US equity on NYSE, NASDAQ, ARCA, AMEX or BATS, active or delisted (OTC excluded).
Each sync rewrites the full history, because adjusted prices change retroactively after every split or
dividend. Requests are paced under the free plan's 200/minute; a full sync takes roughly 30–60 minutes.

Safety: both files are staged and replace the previous pair together, only if both downloads finished,
the newest bar is recent, and neither file lost more than 10% of its rows or symbols (`--force` overrides
the size check). `manifest.json` records what the committed pair contains. A lock prevents concurrent
syncs. Symbols the API rejects are isolated and skipped, up to `--max-rejected` (default 50) per file;
beyond that the request itself is assumed wrong and the run aborts. `marketdata info <file>` summarizes a
Parquet file.

Layout: rows are ordered by date, then symbol, in row groups of about two weeks of the whole market, so a
replay streams the file front to back and date filters skip whole row groups. Alpaca delivers history
symbol by symbol, so the writer buckets rows by calendar quarter, spills them to disk as it goes, and sorts
one quarter at a time; memory stays bounded however long the history. Files written before this layout are
refused by the streaming replay; `marketdata info` reports the layout, and a sync rewrites them.

Known limits:
- Delisted coverage is only as complete as Alpaca's inactive list and history, so some survivorship bias
  remains; a reused ticker maps to whichever company Alpaca associates with it.
- The backtest's `--min-price` filter uses as-traded prices from the raw file (see "Evaluating a strategy
  honestly"); without one it falls back to adjusted prices and warns.
- `backtest` streams bars (with their as-traded closes merged in) from disk, so the whole universe fits;
  `walk-forward` and `paper` still hold the selected bars in memory, so narrow those with
  `--start/--end/--symbols`.

## Fundamentals

`fundamentals sync` (needs `SEC_USER_AGENT="Your Name you@example.com"`; the SEC blocks anonymous
clients) writes to `data/fundamentals/`:

| File | Contents |
|---|---|
| `facts.parquet` | ~30 core XBRL concepts (revenue, earnings, EPS, assets, debt, cash flow, shares) from every 10-K/10-Q/20-F/40-F, one row per reported value **per filing**, with its `filed` date |
| `tickers/<date>.json` | The SEC's CIK→ticker map, kept daily (it only reflects current tickers) |
| `manifest.json` | What the committed file contains |

Restatements and the prior-period comparatives that every filing repeats are all kept, each with the
date it became public. A fact becomes usable at midnight New York time after its filing date (the bulk
data has no time of day, and EDGAR accepts filings until 22:00), both as a backtest event and in
`fundamentals show AAPL --as-of 2024-12-31`, which shows what was known at the start of that day. Same-day
duplicates (an amendment filed with the original) resolve to the later accession. Values come per
reported period: quarterly, year-to-date and annual figures share a period end and are told apart by
`period_start`; fourth quarters are not reported separately (derive them as annual minus nine months).

The SEC's ~1 GB bulk file is fetched with `If-Modified-Since`, so an unchanged night costs one request;
results are staged and only replace the previous file if non-empty, fresh, and not much smaller.

`stocktrader symbols` (`deploy/collector.sh check-symbols` on the server) compares the newest Alpaca asset
list with the SEC ticker map and the facts: how many tradable symbols have a CIK, how many have an annual
net income (what `quality-value` can rank), symbols that differ only in punctuation, and a sample of the
unmatched (mostly ETFs and funds, which file no 10-K). Run it after the first syncs.

**Known limit (important for backtests):** tickers come from the SEC's *current* CIK→ticker map. Applied
to history, a ticker that once belonged to another company gets this company's fundamentals, renamed
companies lose their old symbol, and delisted companies have none. Daily ticker snapshots are kept from
deployment onward so symbols can later be resolved as of each date. Concept coverage also varies by filer.

### Trading on fundamentals

`--fundamentals data/fundamentals` merges filed financials into a `backtest`, `walk-forward` or `paper`
replay, for the symbols that have bars and from two years before the first bar, so annual reports are
already known when trading starts. Only the concepts strategies use are read.

```
cargo run --release -p st-cli -- walk-forward --strategy quality-value --fundamentals data/fundamentals \
    --parquet data/marketdata/bars_daily_all.parquet --start 2018-01-01
```

`quality-value` ranks the market on the first session of each month and holds the `--top-n` (20) best at
equal weight (1/N each; `--weight` doesn't apply). The rank is the sum of two ranks from the latest filings:
earnings yield (annual net income over shares outstanding times the as-traded price) and return on assets
(annual net income over total assets). Only stocks at $5 or more with $5M average daily dollar volume over
20 sessions, positive assets and shares, and reports less than 15 months old are eligible. Walk-forward
tries 10, 20 and 40 names. Simplifications to know: annual figures only (no trailing-twelve-month
stitching), a split after the latest share count misstates market cap until the next filing, share counts
of multi-class issuers may cover one class, and the ticker-mapping limit above applies. It is a first,
deliberately plain factor strategy to measure against SPY, not a claim of an edge.

Strategies that compare symbols get every bar of a session before deciding: the engine calls
`Strategy::after_batch` once per timestamp, after all of that timestamp's events, in backtests and in the
paper planner alike.

## Social signals

`signals apewisdom` rebuilds `data/signals/apewisdom.parquet` from the raw archive (both `.jsonl` and
compacted `.jsonl.gz`): one row per ticker per ApeWisdom snapshot with rank, 24h mentions, net upvotes
(can be negative), and the same figures 24h earlier. Each row is stamped with the time its page was
fetched, so in a backtest it becomes an `Attention` event exactly when we knew it. It is derived data:
nightly housekeeping rebuilds it, and backups skip it. `signals show TSLA` prints a ticker's history.

```
cargo run -p st-signals -- apewisdom
cargo run -p st-cli -- backtest --strategy attention-spike --attention data/signals/apewisdom.parquet \
    --parquet data/marketdata/bars_daily_all.parquet
```

`attention-spike` buys when mentions are at least `--spike-ratio` (3) times the previous 24h and at least
`--spike-min-mentions` (50), and sells `--spike-hold-days` (5) after the latest spike. It shows the
pipeline end to end; it is not a validated strategy. Real backtests need weeks of collected attention
data overlapping the price history.

## Trading plans

A plan is a named, saved description of how to trade, kept as a TOML file in `plans/`: what it's for in your
own words, the strategy and its parameters, which stocks it may trade, its risk limits and starting capital.
Keep several and compare them; the ones that hold up get paper-traded (next step: one paper account per
plan). `plans/quality-value-core.toml` is a commented template.

```
stocktrader plans list                               # every plan, and whether each file is valid
stocktrader plans backtest quality-value-core        # metrics vs the benchmark + reports/quality-value-core.html
stocktrader plans compare                            # all plans over the same dates + reports/compare.html
stocktrader plans backtest trend-megacaps --synthetic   # try a price-only plan before any data is downloaded
```

On the server: `deploy/collector.sh plans list` (and so on); reports land in `data/reports/`.

| Section | Keys |
|---|---|
| top level | `name` (must match the file name), `notes`, `mode` (`backtest` or `paper`), `capital` |
| `[strategy]` | `kind` = `quality-value` (`top_n`), `sma` (`fast`, `slow`, `weight`), `attention-spike` (`min_mentions`, `ratio`, `hold_days`, `weight`) |
| `[universe]` | `symbols` (empty = whole market), `min_price` (as traded), `min_dollar_volume` (20-session average) |
| `[risk]` | `max_position`, `max_gross_exposure`, `max_daily_loss` (fractions of equity; defaults 0.10, 1.0, 0.03) |
| `[backtest]` | `start`, `end` (TOML dates, e.g. `2018-01-01`), `benchmark` (default SPY, or `none`), `slippage_bps` |

Unknown keys are errors, so a typo never silently falls back to a default; inconsistent settings (a fast
average not below the slow one, `top_n` too small for the position cap) are refused. `compare` uses one
window for every plan: the latest start and earliest end among them, unless `--start/--end` are given.
Indicators warm up inside the window: an `sma` plan with `slow = 50` trades from about its 50th session.

The HTML report is one self-contained file (no external scripts or fonts): summary against the benchmark,
equity and drawdown charts with hover readouts, monthly returns, rejected signals by reason, recent fills,
and the plan file itself. Light and dark themes follow the system setting.

## Paper trading

`stocktrader paper` runs once per session, before the open, against an Alpaca **paper** account (the
client refuses any other endpoint). It replays the last `--history-days` (400) of bars, plus attention
if `--attention` is given, through the strategy and risk manager, takes each symbol's latest target, and
reconciles it with the account's real cash, positions and open orders using the same risk checks and order
sizing as the backtester. Orders are whole-share market orders for the day, so they fill at the open, as
simulated fills do. Nothing is kept between runs except the journal: every run starts from what the broker
reports.

```
cargo run --release -p st-cli -- paper --symbols AAPL,MSFT,NVDA --strategy sma            # print the plan
cargo run --release -p st-cli -- paper --symbols AAPL,MSFT,NVDA --strategy sma --submit   # send it
```

Guards:
- Dry run unless `--submit`. Both modes append the plan to `data/live/journal/<date>.jsonl`; submissions
  add one line per order with the broker's answer.
- Runs only before or during a session; after the close, on weekends and on holidays it does nothing.
  Submitting while the market is open needs `--allow-open`, since orders would fill at once rather than
  at the open.
  It refuses market data that doesn't end with the previous session, and an account that isn't `ACTIVE`.
- A file `data/live/KILL` blocks every order that adds exposure; exits still go out. Its first line is
  shown as the reason. Delete it to resume.
- Order ids are `st-<date>-<symbol>-<side>`, and open orders count as pending, so running twice on the same
  day sends nothing new; the broker rejects a repeated id outright.
- `--max-orders` (20) refuses a run that would place more orders than that, and the daily loss limit
  compares equity with the broker's previous close.
- Positions in symbols outside `--symbols`, or without a bar for the previous session, are reported and
  left alone.

On a server, put the flags in `.env` as `PAPER_ARGS` (see `.env.example`), check the plan with
`deploy/collector.sh paper-trade`, then `deploy/collector.sh install-paper-timer` submits each weekday
at 08:45 New York time, after the nightly market data sync. Each submitting run sends a short report to
`ALERT_WEBHOOK_URL` (equity, orders, rejections, broker answers; nothing on market holidays), and a failed
run sends its error lines.

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
deploy/collector.sh sync-marketdata   # download universe + price history now (needs Alpaca keys)
deploy/collector.sh install-timer     # run that nightly (Tue-Sat 03:00 New York) via a systemd user timer
deploy/collector.sh sync-fundamentals # SEC fundamentals now (needs SEC_USER_AGENT)
deploy/collector.sh install-fundamentals-timer  # run that daily at 06:00 New York
```

Both services use one image (`vchance-stocktrader`). A sync rebuilds it from the current checkout, and
the collector picks that build up on its next `up`/`update`. Timer runs log to the user journal:
`journalctl --user -u stocktrader-marketdata`.

The archive lands in `~/stocktrader/data/raw/`, owned by the deploying user. Always go through the script:
plain `docker compose` commands refuse to run without the UID/GID it sets. The container is read-only apart
from `data/`, drops all capabilities, and is capped at 512 MB and one CPU.

### Protecting the archive

The raw archive cannot be re-collected, so the host watches and copies it:

```
deploy/collector.sh install-ops    # hourly `health`, nightly `housekeeping` (01:30 UTC)
deploy/collector.sh health         # per-source freshness, collector container, last backup
deploy/collector.sh housekeeping   # gzip day files older than yesterday, then `backup`
```

- **Alerts** go to `ALERT_WEBHOOK_URL` in `.env` (plain-text POST with a `Title` header; an
  [ntfy.sh](https://ntfy.sh) topic works with its phone app). Health alerts fire when the set of problems
  changes, including recovery, and repeat daily while a problem persists. Without a URL, problems are
  only logged (`journalctl --user -u stocktrader-health`).
- **A source is stale** when it has gone longer than its slowest backed-off polling gap plus two intervals
  (at least 30 minutes) without a record, or if its newest record is in the future. A source that has
  data but lost its keys shows DISABLED (if you retire a source on purpose, move its directory out of
  `data/raw` to stop the daily alert). The market data sync is stale after 3 days; a configured backup
  after 2 days. Once their timers are installed, scheduled jobs are checked too: a market data sync that
  has never completed is MISSING, fundamentals are stale after 2 days without a new ticker snapshot and
  `facts` after 7 days without a new facts file (the SEC normally publishes every weekday), and paper
  trading after 5 days without a journal entry (a Monday holiday makes Friday to Tuesday normal). A timer
  installed less than that long ago that hasn't produced anything yet shows as waiting, not MISSING.
- **Dead-man's switch:** set `HEARTBEAT_URL` (e.g. a healthchecks.io check) and every hourly health run
  pings it, so you hear about it if the host or its timers stop entirely.
- **Backups** go to `BACKUP_DEST` (any rsync destination; remote ones need key-based ssh). A local
  destination must contain a `.stocktrader-backup` file, so an unmounted drive fails loudly. Everything
  in `data/` is copied except the re-downloadable market data Parquet files. Nothing is ever deleted
  from the backup except an uncompressed day file once its compressed copy is there, at most 50 per run
  (`BACKUP_MAX_DELETE`), so a local mistake can't wipe the backup. After compacting a large backlog for
  the first time, the backup reports that it needs more deletions; run it once with a higher limit.
- **Compaction** gzips day files, verifies each copy byte for byte, and only then removes the original.
  It never overwrites an existing `.gz`; a conflicting leftover is reported and left for you. Archive
  readers must accept both `.jsonl` and `.jsonl.gz`.

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
| `st-strategies` | Strategies: `QualityValue` (monthly fundamentals ranking), `AttentionSpike`; `SmaCrossover` is a plumbing test only |
| `st-paper` | Alpaca paper-account client and the pre-open planner (`stocktrader paper`) |
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
1. ~~Alpaca daily bars, universe snapshots, Parquet store~~ (`marketdata`), ~~raw prices for the price
   filter~~, ~~date-major files streamed into backtests~~.
2. ~~Start archiving news and social feeds~~ (`collector`, running on `jabrony`). Next: compress old files.
3. ~~EDGAR fundamentals with point-in-time filing dates~~ (`fundamentals`), ~~in backtests and a
   quality-value strategy~~ (`--fundamentals`, `quality-value`). Next: historical ticker mapping, trailing
   twelve-month figures, and a walk-forward verdict on real data.
4. ~~Mention velocity from ApeWisdom~~ (`signals`, `attention-spike`). Next: Reddit and news text pipeline
   (ticker extraction, dedupe, sentiment scoring via FinBERT/ONNX or an LLM) once those sources have keys.
5. ~~Daily paper trading on Alpaca~~ (`stocktrader paper`: reconciles with the broker each run, shared
   sizing, file kill switch). Next: compare paper fills with the backtest's simulated fills; an intraday
   runner (async fills, partials, cancels) only if a strategy needs one; live trading behind an explicit
   opt-in once paper results justify it.
6. ~~Benchmark comparison and walk-forward evaluation~~ (`--benchmark`, `walk-forward`). Next: stream
   walk-forward folds from disk too (one pass per candidate and fold) so it scales to the whole universe;
   dashboard.
