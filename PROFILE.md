# PROFILE.md — stocktrader

| Field | Value |
|---|---|
| Repository | vchance/stocktrader · default branch `main` · visibility **public** · GitHub plan Free (public repos can have rulesets; none exist today: `gh api repos/vchance/stocktrader/rulesets` returned 0 on 2026-10-31) |
| Classification | active product (single-user US-equity research and paper-trading system; README.md). Young: 30 commits, all 2026-10-27 (audit `evidence/repos-B.md` › stocktrader) |
| Accountable owner | Von Chance (vchance) — approver of exceptions and owner-reserved decisions (no merge-acceptance step since core 0.1.9); sole operator of the server `jabrony` and of the Alpaca, SEC, Reddit and Finnhub accounts |
| Supported platforms | Linux Docker host `jabrony` (README › Running on a server; `Dockerfile`, `compose.yaml`, `deploy/collector.sh`) = `manual` (owner); developer machine via `cargo` (README top) = `manual`. Linux x86_64 build, clippy and tests = `CI` (`ci` job, hosted `ubuntu-24.04`); macOS and the Docker image are not built in CI |
| Toolchain | Rust, edition 2024, workspace resolver 3 (`Cargo.toml`), `Cargo.lock` tracked. No `rust-toolchain.toml`; the image builds with `rust:1.97-slim-trixie` (`Dockerfile:5`) and runs on `debian:trixie-slim`. No `rustfmt.toml`/`clippy.toml`; clippy is enforced by the `ci` job (stable Rust), format is not enforced (the tree is not rustfmt-clean) |
| Required services for tests | none external: HTTP clients are tested against `wiremock` loopback servers (`crates/{paper,marketdata,fundamentals,collector,cli}`), and `check_base_url` (`crates/paper/src/alpaca.rs:150`) admits loopback for that purpose. Whether any test reaches the network was not verified (audit: unknown). No CI provides anything |

## Components and critical paths
Consumers are the reverse dependencies from each crate's `Cargo.toml` (transitive). Test counts are `#[test]`/`#[tokio::test]`
attributes counted by grep on 2026-10-31 (171 in all; the audit recorded ~159), not a test run.

| Component (path glob) | Owner | Critical? | Tests that cover it (command) | Consumers (transitive) |
|---|---|---|---|---|
| `crates/core/**` (`st-core`: domain types, `Strategy`/`ExecutionAdapter` traits) | owner | yes (shared by every crate) | `cargo test -p st-core` (2) | all ten other crates |
| `crates/data/**` (`st-data`: replay, CSV, synthetic data) | owner | yes (point-in-time replay) | `cargo test -p st-data` (1) | st-engine, st-marketdata, st-paper, st-cli |
| `crates/risk/**` (`st-risk`: position cap, exposure, daily loss halt, kill switch, filters) | owner | **yes (order path)** | `cargo test -p st-risk` (9) | st-engine, st-marketdata, st-paper, st-cli |
| `crates/strategies/**` (`st-strategies`) | owner | yes (decides targets) | `cargo test -p st-strategies` (19) | st-engine, st-marketdata, st-paper, st-cli |
| `crates/engine/**` (`st-engine`: backtester, `SimulatedBroker`, metrics) | owner | yes (sizing shared with paper) | `cargo test -p st-engine` (15, incl. `tests/`) | st-marketdata, st-paper, st-cli |
| `crates/paper/**` (`st-paper`: Alpaca trading client, pre-open planner, journal) | owner | **yes (places broker orders; holds API keys in headers)** | `cargo test -p st-paper` (19) | st-cli |
| `crates/marketdata/**` (`st-marketdata`: Alpaca bars/assets sync, Parquet store) | owner | yes (all backtest data; uses Alpaca keys) | `cargo test -p st-marketdata` (24, incl. `tests/`) | st-cli |
| `crates/fundamentals/**` (`st-fundamentals`: SEC XBRL sync) | owner | yes (point-in-time fundamentals) | `cargo test -p st-fundamentals` (15) | st-cli |
| `crates/collector/**` (`st-collector`: news/social archiver; `src/config.rs` reads keys from env) | owner | **yes (the raw archive cannot be re-collected; README › Data collector)** | `cargo test -p st-collector` (22) | st-signals, st-cli |
| `crates/signals/**` (`st-signals`: ApeWisdom attention Parquet) | owner | no (derived, rebuilt nightly) | `cargo test -p st-signals` (7) | st-cli |
| `crates/cli/**` (`st-cli`, binary `stocktrader`: backtest, walk-forward, paper, plans, options-lab) | owner | **yes (`--submit` paths in `src/paper.rs`, `src/plans.rs`; per-plan keys)** | `cargo test -p st-cli` (38, incl. `tests/`) | none (binary) |
| `plans/**` (TOML trading plans; `_research.toml` sets the hold-out) | owner | yes when `mode = "paper"` (all eight are `backtest` today: one sets it, seven omit the key and get the default, `crates/cli/src/plans.rs:132`) | `cargo run -p st-cli -- plans list` (validates every file) + `cargo test -p st-cli` | the paper timer on `jabrony` (`plans paper`) |
| `deploy/collector.sh`, `Dockerfile`, `compose.yaml`, `.dockerignore`, `.env.example` | owner | **yes (production host: collector, timers, backups, paper submission)** | none automated; `bash -n deploy/collector.sh`; owner-run on `jabrony` | the running collector and timers |
| `README.md` | owner | no | review only | readers |

## Check selection (impact-based; core D2)
The `ci` job (`.github/workflows/ci.yml`) runs on every PR and on push to `main`: `cargo clippy --workspace
--all-targets --locked -- -D warnings`, `cargo test --workspace --locked` (the full suite, so it covers every selection
row below) and `cargo build --workspace --release --locked`, on the latest stable Rust. It does not run `cargo fmt
--check` (the tree is not rustfmt-clean yet), `plans list`, `bash -n deploy/collector.sh` or anything on `jabrony`; those
stay **manual with evidence** (date, SHA, operator, exit code, test count from the `test result:` lines), and the R5
fallback is the commands in `AGENTS.md › Commands`. Anything not run is recorded `unavailable`, never `passed`. **Every
PR that changes anything outside prose docs is classified high** (AGENTS.md › A1; Merge authority): the rule was
written for "until a CI test job exists"; the job now exists and the rule stays until the owner decides otherwise.

| Changed paths | Checks that run on the PR | Evidence recorded as |
|---|---|---|
| `README.md`, other `**/*.md` except `AGENTS.md`, `PROFILE.md` | PR review (core D6); no doc linter is configured | `docs: reviewed`; build/test `n/a: docs only` |
| `AGENTS.md`, `PROFILE.md`, `CLAUDE.md`, `.agent-rules/**`, `.githooks/**`, `.claude/**`, `.github/**` | `policy-drift` + `pr-evidence` + `ci` (CI); **high tier** (R9) | per check |
| `crates/cli/**` | `ci` (clippy + `cargo test --workspace`, which includes `cargo test -p st-cli`) + manual fmt (AGENTS.md › Commands) | `ci: passed (N tests) @ sha (CI run URL)`; fmt `@ sha (manual, date, operator)` |
| `crates/signals/**` | `ci` (clippy + `cargo test --workspace`, which includes `cargo test -p st-signals -p st-cli`) + manual fmt | as above |
| `crates/fundamentals/**` | `ci` (clippy + `cargo test --workspace`, which includes `cargo test -p st-fundamentals -p st-cli`) + manual fmt | as above |
| `crates/marketdata/**` | `ci` (clippy + `cargo test --workspace`, which includes `cargo test -p st-marketdata -p st-cli`) + manual fmt | as above |
| `crates/collector/**` | `ci` (clippy + `cargo test --workspace`, which includes `cargo test -p st-collector -p st-signals -p st-cli`) + manual fmt; security playbook if `src/config.rs` or a source's auth changes | as above |
| `crates/paper/**` | `ci` (clippy + `cargo test --workspace`, which includes `cargo test -p st-paper -p st-cli`) + manual fmt; **security playbook always** | as above |
| `crates/engine/**` | `ci` (clippy + `cargo test --workspace`, which includes `cargo test -p st-engine -p st-marketdata -p st-paper -p st-cli`) + manual fmt | as above |
| `crates/core/**`, `crates/data/**`, `crates/risk/**`, `crates/strategies/**` | **escalation: full suite** `cargo test --workspace`, run by `ci` (each is consumed by engine, marketdata, paper and cli); `crates/risk/**` also triggers the security playbook | per check |
| `plans/**` | manual: `cargo run -p st-cli -- plans list` (every file valid) + `ci` (tests); a plan whose `mode` becomes `paper`, or a change to `plans/_research.toml` `holdout_start`, is an Owner decision | per check |
| `deploy/**`, `Dockerfile`, `compose.yaml`, `.dockerignore`, `.env.example` | manual: `bash -n deploy/collector.sh`; image build and `deploy/collector.sh` runs only by the owner on `jabrony` (agents do not run Docker there); high tier | pasted evidence (date, SHA, operator) |
| `Cargo.toml`, `Cargo.lock`, any `crates/*/Cargo.toml` | **escalation: full suite** `cargo test --workspace` + clippy (both run by `ci`) + manual fmt | per check |
| Uncertain mapping / more than two crates / cross-crate refactor | **escalation: full suite** | per check |

Escalation triggers (any one → `cargo test --workspace`): shared crate (`core`, `data`, `risk`, `strategies`),
dependency or toolchain change (`Cargo.toml`, `Cargo.lock`, `Dockerfile` base image), broad refactor, uncertain impact,
anything the owner will deploy to `jabrony`, a regression that escaped selection.

## Lanes
Runner note: `policy-drift` and `pr-evidence` run on `fromJSON(vars.RUNS_ON_SMALL || '["ubuntu-24.04"]')`. No repo
variable is set (`gh variable list --repo vchance/stocktrader` empty, 2026-10-31), so they run on GitHub-hosted
`ubuntu-24.04` (free for public repositories). Owner action: set `RUNS_ON_SMALL` only if a different runner is wanted
(GitHub setting; not changed by this PR).

| Lane | What runs | When | Owner of a failure |
|---|---|---|---|
| PR (fast) | `pr-evidence` on every PR; `policy-drift` when policy paths change; `ci` (Integration batch row) | every PR commit | author |
| Integration batch | `ci` (`.github/workflows/ci.yml`): clippy `-D warnings`, `cargo test --workspace`, `cargo build --workspace --release` (all `--locked`) on hosted `ubuntu-24.04` via `RUNS_ON_SMALL`, `permissions: contents: read`, `timeout-minutes: 45`, no secrets. `cargo fmt --all --check` is left out until the tree is reformatted | every PR commit and every push to `main` | author (PR); merger (`main`) |
| Scheduled | none. Proposed — owner decision: weekly `cargo audit` (or `cargo deny`) once the CI job exists | — | owner |
| Release | none automated; deployment is the owner's `deploy/collector.sh update` on `jabrony` (Release and operations) | owner | owner |
| Manual with evidence | what `ci` does not run: `cargo fmt --all --check`, `cargo run -p st-cli -- plans list`, `bash -n deploy/collector.sh`, image build; owner-run `deploy/collector.sh paper-trade` dry run (no `--submit`) after a deploy that touches the order path | per PR that touches the paths; pasted in the PR with date, SHA, operator, exit code, test count | author (owner for `jabrony`) |

## Review triggers
| Playbook | Triggered by paths/changes |
|---|---|
| ux-review | `crates/cli/src/report.rs` (self-contained HTML report, light/dark themes; README › Testing plans honestly) and its rendered output in `reports/`; CLI output formats others parse (`deploy/collector.sh` awk filter on runner report lines, `paper_trade`) |
| technical-design | new strategy or plan key, change to point-in-time rules (`available_at`, filing-date usage; README › Rules that keep backtests honest), Parquet layout or manifest format, journal format, new data source |
| architecture | new broker (IBKR, README › Decisions), an intraday or live runner, new storage or service, a new crate or cross-crate boundary, adding CI |
| security | **order placement** (`crates/paper/**`, `crates/cli/src/paper.rs`, `crates/cli/src/plans.rs` `--submit`/`--allow-open` paths, `crates/risk/**`, any `plans/*.toml` `mode` change); **broker and API auth** (`crates/paper/src/alpaca.rs` incl. `check_base_url`, `crates/marketdata/src/alpaca.rs`, `crates/collector/src/sources/**`, per-plan `APCA_API_KEY_ID_<PLAN>` handling); **config loading** (`crates/collector/src/config.rs`, `dotenvy`/env reads in `crates/*/src/main.rs` and `crates/cli/src/*.rs`, `.env.example`, `compose.yaml` `env_file`, `deploy/collector.sh` `.env` handling and alert-report filter); container hardening in `compose.yaml`/`Dockerfile`; dependency changes; `.github/**`, `.claude/**` |
| performance | streaming replay and Parquet sync memory (`crates/marketdata/**`, `crates/engine/**`; container capped at 512 MB/1 CPU in `compose.yaml`); budgets: to measure |
| observability | `deploy/collector.sh` `health`, alerts (`ALERT_WEBHOOK_URL`), heartbeat, journals under `data/live/`, systemd timer logs (`journalctl --user -u stocktrader-*`) |

## Merge authority
- Low: `.agent-rules/merge.sh <n>` once core R3–R6 hold (D5). Only prose-docs PRs can be low.
- **Every PR that changes anything outside prose docs is classified high** (AGENTS.md › A1, stricter than core §3):
  since core 0.1.9 the wrapper merges every tier, so this rule sets review depth, not who merges. The `ci` job now
  exists and verifies clippy, the workspace tests and the release build on the head SHA; fmt and the remaining manual
  checks are verified by the evidence recorded in the PR and the independent reviewer's check of it. A1 stays until the
  owner decides otherwise.
- High: `.agent-rules/merge.sh <n>` once core R3–R6 hold and the independent review verdict `ready` for the head is recorded (D5); no owner acceptance step (owner decision 2026-11-01). Emergency: declared only by the owner (vchance), never by the agent; the PR names the declarer; follow-up PR within 7 days.
- Server-side enforcement available: **rulesets are possible (public repository) but none are configured.** Owner
  action (not done by this PR): add a ruleset on `main` requiring a pull request and the `pr-evidence` check (and the
  `ci` check), blocking force-pushes and deletion (audit F2). Until then the mechanisms are the
  merge wrapper, the pre-push hook and the three workflows.

## Exceptions (governed)
R-rule gaps only (core §1): what the repository cannot yet satisfy, recorded with authority and expiry. A deviation from a
D-rule is an override, not an exception: in force only as an `overrides: Dn` block in `AGENTS.md › Overrides` (core §5.2).
| Id | Rule | Replacement | Rationale | Authority | Scope | Expiry/review |
|---|---|---|---|---|---|---|
| — | — | none | — | — | — | — |

### Proposed D-rule overrides (not in force)
Moved into `AGENTS.md › Overrides` only once the owner accepts it; until then the core default stands as written.
- proposed `overrides: D1` — the reviewer runs the `cargo test` selection from Check selection (and clippy) on the PR head and says what it ran — why: there is no CI evidence to reuse (audit F15, `06-worked-examples.md` §5) — authority: owner decision pending — scope: `crates/**`, `plans/**`, `Cargo.toml`, `Cargo.lock` — review: when the proposed CI job exists, or 2027-01-31.
- D5 needs no override: since core 0.1.9 the wrapper merges every tier; the high-tier rule in Merge authority sets review depth for non-docs PRs, and the recorded manual evidence plus the independent review are the verification.

## Release and operations (core R11)
| Field | Value |
|---|---|
| Versioning | workspace `version = "0.1.0"`, `publish = false` (`Cargo.toml`); no git tags. Not bumped per change. Proposed — owner decision: tag the commit deployed to `jabrony` |
| Changelog / release notes | none; the README's Roadmap and Decisions tables record state. PR descriptions are the record |
| Release procedure | owner-only deploy: after the PR merges to `main` (agent, `.agent-rules/merge.sh`, D5), the owner runs on `jabrony` `deploy/collector.sh update` (`git pull --ff-only` + rebuild + restart the collector; `deploy/collector.sh:71-73`). Timer jobs (`sync-marketdata`, `sync-fundamentals`, `paper-trade`) rebuild the image from the server checkout on their next run, so a pulled change reaches the paper timer at its next 08:45 New York run (`deploy/collector.sh:519`, `:555`) |
| Deploy targets | Docker on `jabrony`: collector service (restart unless-stopped) and one-shot jobs from the same image `vchance-stocktrader`; systemd user timers for market data (Tue–Sat 03:00 NY), fundamentals (06:00 NY), paper trading (Mon–Fri 08:45 NY), health/housekeeping. No crates.io publish |
| Rollback | revert the PR on `main`, then `deploy/collector.sh update` on `jabrony`. To stop trading immediately: create `data/live/KILL` (blocks new exposure; exits still go out; README › Paper trading). Data files are replaced only as staged pairs (README › Market data) and the raw archive is backed up to `BACKUP_DEST`. Never tested as a drill: to confirm |
| Post-release watch | `deploy/collector.sh health` and `status`; the next paper run's report on `ALERT_WEBHOOK_URL`; `journalctl --user -u stocktrader-marketdata` after the next sync. One trading day for order-path changes (owner) |
| Dependency cadence | none today; dependency bumps and majors are Owner decisions. Proposed: weekly audit once CI exists (Lanes) |
| Deprecation policy | n/a (single user; plan-file keys are validated, unknown keys are errors: README › Testing plans honestly) |

## Cleanup (core R10)
| Resource this repo's work creates | Where | Teardown command | Safe to remove when |
|---|---|---|---|
| Build output | `target/` (git-ignored) | `cargo clean` (or `cargo clean -p <crate>`) | always (rebuilds) |
| Local data written by runs | `data/`, `reports/`, `research/` (git-ignored) | remove only what this session created; `research/experiments.jsonl` is the experiment log (README › Testing plans honestly): never delete it | owner decision for anything not created by the session |
| Test temp dirs | `tempfile` dirs under the OS temp dir | removed by the tests themselves | always |
| Containers / images (owner, `jabrony`) | `vchance-stocktrader` image, `stocktrader-collector` compose project | `deploy/collector.sh stop` (owner); agents do not run Docker on `jabrony` | owner decision |
| Worktrees | `.claude/worktrees/*` | `git worktree remove <path> && git worktree prune` | branch merged, `git status` clean |
| Stale branches | 21 local `worktree-*` branches in the main checkout (counted 2026-10-31; audit F16) | owner triage; agents do not delete them | owner decision |
| CI runner artifacts | `ci` job's rust-cache entries (GitHub Actions cache, written only from `main`) | evicted by GitHub (7 days unused / 10 GB) | always |
| Cloud / paid | Alpaca paper accounts, healthchecks/ntfy endpoints (owner's) | none created by agents | n/a |

## Budgets
- CI: policy checks take seconds per PR commit; the `ci` job's duration is to measure (cap `timeout-minutes: 45`).
- Performance: to measure (collector container limits 512 MB / 1 CPU in `compose.yaml`; full market-data sync ~30–60 min, README › Market data).
