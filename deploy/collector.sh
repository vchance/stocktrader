#!/usr/bin/env bash
# Manage stocktrader's data services on a Linux host with Docker. Run from anywhere.
#
#   deploy/collector.sh up               build from the current checkout and (re)start the collector
#   deploy/collector.sh update           git pull --ff-only, then up
#   deploy/collector.sh status           container state and enabled sources
#   deploy/collector.sh logs             follow collector logs
#   deploy/collector.sh stop             stop the collector (data and .env are kept)
#   deploy/collector.sh sync-marketdata  download the universe and full daily price history now
#                                        (extra flags go to `marketdata sync`, e.g. --force)
#   deploy/collector.sh health           check every source is producing; alert on changes
#   deploy/collector.sh housekeeping     compress old archive files, then back up
#   deploy/collector.sh backup           copy data/ to BACKUP_DEST (set in .env)
#   deploy/collector.sh install-ops      hourly health check + nightly housekeeping (systemd user timers)
#   deploy/collector.sh install-timer    nightly sync-marketdata (systemd user timer)
#                                        (install-ops/install-timer DIR only write the units to DIR)
set -euo pipefail

SYNC_CONTAINER=stocktrader-marketdata-sync
# Exit status when another sync is already running; the systemd unit treats it as success.
ALREADY_RUNNING=75
# Re-send an unchanged alert after this long, so a lingering problem isn't forgotten.
REALERT_SECS=$((24 * 3600))
# A configured backup older than this counts as a problem.
BACKUP_MAX_AGE_SECS=$((2 * 24 * 3600))

# Wrapped in main and exited explicitly: `update` rewrites this file mid-run, and bash would otherwise
# keep reading the new file from its old offset.
main() {
    local self
    self="$(readlink -f "$0")"
    cd "$(git -C "$(dirname "$self")" rev-parse --show-toplevel)"
    export COLLECTOR_UID COLLECTOR_GID
    COLLECTOR_UID="$(id -u)"
    COLLECTOR_GID="$(id -g)"

    case "${1:-}" in
        up)
            prepare
            docker compose up -d --build
            docker compose ps
            ;;
        update)
            git pull --ff-only
            exec "$self" up
            ;;
        status)
            docker compose ps
            docker compose run --rm --no-deps collector sources
            ;;
        logs)
            docker compose logs -f --tail=100 collector
            ;;
        stop)
            docker compose stop collector
            ;;
        sync-marketdata)
            sync_marketdata "${@:2}"
            ;;
        health)
            health
            ;;
        housekeeping)
            housekeeping
            ;;
        backup)
            prepare
            backup
            ;;
        install-ops)
            install_ops "$self" "${2:-}"
            ;;
        install-timer)
            install_timer "$self" "${2:-}"
            ;;
        *)
            sed -n '2,16p' "$self"
            return 1
            ;;
    esac
}

prepare() {
    # Created here, not by Docker, so they belong to this user rather than root.
    mkdir -p data
    if [[ ! -e .env ]]; then
        install -m 600 .env.example .env
        echo "Created .env from .env.example. Starting with keyless sources only;"
        echo "add API keys to .env, then run: deploy/collector.sh up"
    fi
    chmod 600 .env
}

# Value of KEY from .env (last assignment wins), or empty.
env_value() {
    sed -n "s/^$1=//p" .env 2>/dev/null | tail -n 1 | tr -d '\r' | sed 's/[[:space:]]*$//'
}

# Posts an alert to ALERT_WEBHOOK_URL (ntfy-style: plain-text body, Title header). Never fails the caller.
notify() {
    local title="$1" body="$2" url
    url="$(env_value ALERT_WEBHOOK_URL)"
    if [[ -z "$url" ]]; then
        echo "ALERT_WEBHOOK_URL not set in .env; alert not sent: $title" >&2
        return 0
    fi
    curl -fsS -m 20 -H "Title: $title" --data-binary "$body" "$url" >/dev/null ||
        echo "Failed to deliver alert: $title" >&2
}

sync_marketdata() {
    prepare
    if [[ -n "$(docker ps -q --filter "name=^${SYNC_CONTAINER}\$")" ]]; then
        echo "A market data sync is already running; not starting another." >&2
        return "$ALREADY_RUNNING"
    fi
    docker compose build marketdata
    # Clear only an exited leftover; `docker rm` without -f refuses a running container.
    docker rm "$SYNC_CONTAINER" >/dev/null 2>&1 || true
    docker compose run --rm -T --name "$SYNC_CONTAINER" marketdata sync "$@" &
    local pid=$! status=0
    # On SIGTERM (e.g. a systemd timeout) stop the container this run started, not just the compose client.
    trap 'docker stop "$SYNC_CONTAINER" >/dev/null 2>&1 || true' TERM INT
    wait "$pid" || status=$?
    if ((status > 128)); then
        # Interrupted by the trap: let the client exit now that its container is stopping.
        wait "$pid" || true
    fi
    trap - TERM INT
    return "$status"
}

health() {
    prepare
    local report status=0 problems=""
    if [[ -z "$(docker compose ps -q --status running collector 2>/dev/null)" ]]; then
        problems+="collector NOT-RUNNING"$'\n'
    fi
    report="$(docker compose run --rm --no-deps -T collector health 2>&1)" || status=$?
    echo "$report"
    if ((status != 0)); then
        # Only names and states, so the alert key doesn't change as ages tick up.
        local found
        found="$(awk '$2 == "STALE" || $2 == "MISSING" { print $1 " " $2 }' <<<"$report")"
        problems+="${found:-health-check FAILED}"$'\n'
    fi
    if [[ -n "$(env_value BACKUP_DEST)" ]]; then
        local last=0
        [[ -f data/.last-backup ]] && last="$(cat data/.last-backup)"
        if (($(date +%s) - last > BACKUP_MAX_AGE_SECS)); then
            problems+="backup STALE"$'\n'
            echo "backup         STALE    last success $( ((last)) && date -d "@$last" -u +%FT%TZ || echo never)"
        else
            echo "backup         ok       last success $(date -d "@$last" -u +%FT%TZ)"
        fi
    else
        echo "backup         -        BACKUP_DEST not set in .env"
    fi
    problems="$(sed '/^[[:space:]]*$/d' <<<"$problems" | sort)"
    alert_on_change "$problems" "$report"
    [[ -z "$problems" ]]
}

# Alerts when the set of problems changes (including recovery), and re-alerts an unchanged problem daily.
alert_on_change() {
    local problems="$1" report="$2" state=data/.health-alert now last_time=0 last=""
    now="$(date +%s)"
    if [[ -f "$state" ]]; then
        last_time="$(head -n 1 "$state")"
        last="$(tail -n +2 "$state")"
    fi
    if [[ "$problems" == "$last" ]] && { [[ -z "$problems" ]] || ((now - last_time < REALERT_SECS)); }; then
        return 0
    fi
    printf '%s\n%s\n' "$now" "$problems" >"$state.tmp" && mv "$state.tmp" "$state"
    if [[ -z "$problems" ]]; then
        notify "stocktrader on $(hostname): recovered" "$report"
    else
        notify "stocktrader on $(hostname): $(tr '\n' ' ' <<<"$problems")" "$report"
    fi
}

housekeeping() {
    prepare
    local failed=""
    docker compose run --rm --no-deps -T collector compact || failed+="compaction "
    backup || failed+="backup "
    if [[ -n "$failed" ]]; then
        notify "stocktrader on $(hostname): ${failed}failed" "See: journalctl --user -u stocktrader-housekeeping"
        return 1
    fi
}

# Copies data/ to BACKUP_DEST (any rsync destination: local path, mounted drive, or user@host:path).
# Market data Parquet files are skipped (they are re-downloadable); everything else, including the raw
# archive, asset snapshots and cursors, is copied. --delete only applies to *.jsonl, so the uncompressed
# copy of a day is removed once its .jsonl.gz exists, but nothing else is ever deleted from the backup,
# even if it disappears locally.
backup() {
    local dest
    dest="$(env_value BACKUP_DEST)"
    if [[ -z "$dest" ]]; then
        echo "BACKUP_DEST not set in .env; skipping backup."
        return 0
    fi
    if [[ -z "$(find data/raw -type f -name '*.jsonl*' -print -quit 2>/dev/null)" ]]; then
        echo "data/raw has no archive files; refusing to back up an empty archive." >&2
        return 1
    fi
    rsync -a --partial --delete --filter='P! *.jsonl' \
        --exclude='*.tmp' --exclude='.sync.lock' --exclude='marketdata/*.parquet' \
        data/ "${dest%/}/"
    date +%s >data/.last-backup
    echo "Backed up data/ to $dest"
}

# Writes a oneshot service and its timer. Args: dir name description exec_args timeout calendar
# [extra timer lines].
write_timer_units() {
    local dir="$1" name="$2" description="$3" exec="$4" timeout="$5" calendar="$6" extra="${7:-}"
    cat >"$dir/$name.service" <<EOF
[Unit]
Description=stocktrader: $description

[Service]
Type=oneshot
ExecStart=$exec
TimeoutStartSec=$timeout
EOF
    cat >"$dir/$name.timer" <<EOF
[Unit]
Description=stocktrader: $description

[Timer]
OnCalendar=$calendar
$extra
[Install]
WantedBy=timers.target
EOF
}

enable_timers() {
    systemctl --user daemon-reload
    systemctl --user enable --now "$@"
    systemctl --user list-timers "$@" --no-pager
    if [[ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != "yes" ]]; then
        echo "Warning: lingering is off, so timers only run while $USER is logged in."
        echo "Enable it with: sudo loginctl enable-linger $USER"
    fi
}

install_ops() {
    local self="${1//%/%%}" units="${2:-$HOME/.config/systemd/user}"
    mkdir -p "$units"
    write_timer_units "$units" stocktrader-health "archive health check" "\"$self\" health" 10min \
        "hourly" "RandomizedDelaySec=5m"
    # 01:30 UTC: well after the UTC midnight that closes the previous day file.
    write_timer_units "$units" stocktrader-housekeeping "compress and back up the archive" \
        "\"$self\" housekeeping" 3h "*-*-* 01:30:00 UTC" $'RandomizedDelaySec=15m\nPersistent=true'
    if [[ -n "${2:-}" ]]; then
        echo "Wrote units to $units without installing them."
        return 0
    fi
    enable_timers stocktrader-health.timer stocktrader-housekeeping.timer
}

install_timer() {
    local self="${1//%/%%}" units="${2:-$HOME/.config/systemd/user}"
    mkdir -p "$units"
    # Retries cover a catch-up run firing at boot before Docker is up, or a transient API outage; the
    # start limit stops a persistent failure from retrying all day.
    cat >"$units/stocktrader-marketdata.service" <<EOF
[Unit]
Description=stocktrader: download universe and daily price history
StartLimitIntervalSec=12h
StartLimitBurst=3

[Service]
Type=oneshot
ExecStart="$self" sync-marketdata
# Another sync already running: nothing to do, and nothing to retry.
SuccessExitStatus=$ALREADY_RUNNING
# On timeout the script's SIGTERM trap stops the container it started.
TimeoutStartSec=3h
Restart=on-failure
RestartSec=20min
EOF
    # After each US session (Mon-Fri), once Alpaca has the final daily bars.
    cat >"$units/stocktrader-marketdata.timer" <<EOF
[Unit]
Description=stocktrader: nightly market data sync

[Timer]
OnCalendar=Tue..Sat 03:00 America/New_York
RandomizedDelaySec=15m
Persistent=true

[Install]
WantedBy=timers.target
EOF
    if [[ -n "${2:-}" ]]; then
        echo "Wrote units to $units without installing them."
        return 0
    fi
    enable_timers stocktrader-marketdata.timer
}

main "$@"
exit
