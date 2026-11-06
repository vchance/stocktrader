# Agent instructions — stocktrader

<!-- agent-rules manifest (checked by policy-drift; keep these lines exact) -->
policy: agent-rules/core@0.1.x   ·   profile: PROFILE.md   ·   core-digest: 66f337a232e9

Read, in this order: (1) `.agent-rules/core.md` — the shared standard this repository follows; (2) `PROFILE.md`; (3) the sections below. Then classify the change (core R2) before editing anything.

## This repo
stocktrader: automated US-equity research and trading in Rust (11 crates under `crates/`), with backtest, walk-forward
and **paper** modes, a news/social data collector, and SEC/Alpaca data syncs, deployed with Docker on the owner's server
`jabrony` (README.md). The only order path today is `st-paper`, which places real orders on **Alpaca paper accounts**
and refuses every other endpoint (`crates/paper/src/alpaca.rs:150`); the README's "live" mode is a roadmap item, not code.
CI: the `ci` job (`.github/workflows/ci.yml`) runs clippy (`-D warnings`), `cargo test --workspace` and a release build
on every PR and on push to `main`; `cargo fmt --check` is not in it yet (the tree is not rustfmt-clean). Besides it there
are only the two policy workflows; anything `ci` does not run is manual and recorded with date, SHA, operator and test
count (PROFILE.md › Check selection).

## Commands
```sh
git config core.hooksPath .githooks   # once per clone: enables the pre-push guard (core R3)
cargo build --workspace                                  # build
cargo test --workspace                                   # all tests (escalation only; see PROFILE.md › Check selection)
cargo test -p st-paper -p st-cli                         # one crate and its consumers (selection per PROFILE.md)
cargo clippy --workspace --all-targets -- -D warnings    # lint (enforced by the `ci` job; clean at d8b8eb5)
cargo fmt --all --check                                  # format check (no rustfmt.toml; fails at d8b8eb5, not in `ci`)
cargo run -p st-cli -- plans list                        # validates every plans/*.toml (README › Trading plans)
cargo run -p st-cli -- backtest                          # synthetic data, no keys (README top)
bash -n deploy/collector.sh                              # syntax check only; the script itself is run by the owner on jabrony
```
The commands come from the README and the workspace layout. First run of clippy and fmt (2026-11-01, d8b8eb5, Rust
1.97.1): clippy clean; fmt reports 740 hunks in 62 files, so it stays out of `ci` until a reformat PR (record findings,
do not suppress them, D8).
Never pass `--submit` (see Repository additions).

## Layout
- `crates/core` domain types and traits; `data`, `risk`, `strategies` are shared by `engine`, `marketdata`, `paper` and
  `cli`, so a change there escalates to the full suite (PROFILE.md › Check selection).
- `crates/paper` is the broker boundary: Alpaca trading client (`src/alpaca.rs`, paper URL guard, sensitive key headers),
  planner (`src/plan.rs`) and journal (`src/execute.rs`). `crates/cli/src/paper.rs` and `src/plans.rs` own `--submit`,
  `--allow-open` and per-plan keys `APCA_API_KEY_ID_<PLAN>` / `APCA_API_SECRET_KEY_<PLAN>`.
- `crates/collector` writes the raw archive that cannot be re-collected (`data/raw/`); `crates/signals` derives from it.
- `plans/*.toml` are trading plans; `plans/_research.toml` sets the hold-out start. `mode` defaults to `backtest`.
- `deploy/collector.sh`, `compose.yaml`, `Dockerfile` are the production deployment on `jabrony` (collector service,
  systemd timers, backups, paper submission at 08:45 New York on weekdays).

## Gotchas
- The `ci` job's run on the head SHA is the CI evidence reviewers can reuse (core D1) for clippy, the workspace tests
  and the release build; fmt, `plans list`, `bash -n deploy/collector.sh` and everything on `jabrony` stay manual.
- Tests use `wiremock` loopback servers; `check_base_url` admits `http://127.0.0.1`/`localhost` for that reason. Do not
  widen it.
- Secrets live only in the untracked `.env` on the host (read by `dotenvy` and compose `env_file`); `.env.example` is
  the tracked template. Do not open `.env*` (`.claude/settings.json` denies Read; the deny does not stop `cat` through
  Bash, so the rule is yours to keep), and never put a key on a command line or in a plan file.
- Plan files reject unknown keys; a plan's `name` must match its file name (README › Trading plans).
- `research/experiments.jsonl` (`data/research/` on the server) is the experiment log and hold-out record: never delete
  or rewrite it. Running `plans backtest --holdout` counts as a look at the held-back period: ask first.
- Money is `rust_decimal::Decimal`, never `f64`; every event carries `available_at` (README › Rules that keep backtests honest).
- `deploy/collector.sh update` on the server is `git pull --ff-only` + rebuild; timer jobs rebuild from the server
  checkout, so whatever is on `main` and pulled there is what trades next.

## Repository additions
- **A1** Every PR that changes anything outside prose docs is classified **high** tier (stricter than core §3; this sets
  review depth, the wrapper merges every tier since core 0.1.9; PROFILE.md › Merge authority). The rule was written
  for "until a CI test job exists"; that job now exists (`ci`: clippy, `cargo test --workspace`, release build, no fmt),
  and the rule stays in force until the owner decides otherwise.
- **A2** Agents never run `--submit` (`stocktrader paper`, `stocktrader plans paper`, `deploy/collector.sh paper-trade`),
  never set `--allow-open`, and never create, edit or delete files under `data/live/` (including `KILL`). Dry runs are
  allowed only with keys the owner has already configured; agents do not handle broker keys (audit
  `06-worked-examples.md` §5).
- **A3** Any change that touches order placement, broker or API authentication, or configuration/secret loading
  (paths listed in PROFILE.md › Review triggers › security) triggers the security playbook and records in the PR how
  the paper-only guard, the kill switch, `--max-orders` and the dry-run default still hold.
- **A4** Agents do not run Docker, `deploy/collector.sh` subcommands or anything else on `jabrony`; the owner does,
  and pastes the evidence.

## Owner decisions
- Running anything with `--submit`, installing or changing the paper timer, and switching a plan to `mode = "paper"`.
- Any live-money trading, a second broker, or loosening `check_base_url` in `crates/paper/src/alpaca.rs`.
- Moving `holdout_start` in `plans/_research.toml`, or looking at a plan's held-back period (`--holdout`).
- Deploys to `jabrony` (`deploy/collector.sh update`), timer installs, backup destinations and `BACKUP_MAX_DELETE`.
- Dependency bumps and majors; the Rust toolchain/base image in `Dockerfile`.
- Adding CI (PROFILE.md › Lanes), repository rulesets on `main` (PROFILE.md › Merge authority) and the D1 override
  (PROFILE.md › Exceptions); triage of the 21 stale local `worktree-*` branches.
