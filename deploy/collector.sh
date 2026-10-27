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
#   deploy/collector.sh sync-fundamentals  rebuild SEC company fundamentals if the SEC published new data
#                                        (needs SEC_USER_AGENT in .env; extra flags go to `fundamentals sync`)
#   deploy/collector.sh health           check every source and scheduled job is producing; alert on changes
#   deploy/collector.sh housekeeping     compress old archive files, rebuild derived signals, then back up
#   deploy/collector.sh backup           copy data/ to BACKUP_DEST (set in .env)
#   deploy/collector.sh install-ops      hourly health check + nightly housekeeping (systemd user timers)
#   deploy/collector.sh install-timer    nightly sync-marketdata (systemd user timer)
#   deploy/collector.sh install-fundamentals-timer  daily sync-fundamentals (systemd user timer)
#   deploy/collector.sh paper-trade      plan today's Alpaca paper-account orders with PAPER_ARGS from .env and
#                                        print them; add --submit to send them (extra flags go to `stocktrader paper`)
#   deploy/collector.sh install-paper-timer  paper-trade --submit each weekday before the open (systemd user timer)
#                                        (the install-* commands given a DIR only write the units there)
#   deploy/collector.sh check-symbols    how much of the Alpaca universe the SEC ticker map and fundamentals cover
set -euo pipefail

# Every `docker compose run` reads from /dev/null: run attaches stdin by default and would otherwise swallow
# input meant for whatever invoked this script.
# Name of the container a running sync uses; set by run_sync, read by its SIGTERM trap.
SYNC_CONTAINER=""
# Exit status when another sync is already running; the systemd unit treats it as success.
ALREADY_RUNNING=75
# Re-send an unchanged alert after this long, so a lingering problem isn't forgotten.
REALERT_SECS=$((24 * 3600))
# A configured backup older than this counts as a problem.
BACKUP_MAX_AGE_SECS=$((2 * 24 * 3600))
# Scheduled jobs whose output is older than this count as STALE (once their timer is installed). Paper runs
# on weekdays, so a Monday holiday makes Friday to Tuesday the normal longest gap. (Market data freshness is
# checked by `collector health` from its manifest.)
FUNDAMENTALS_MAX_AGE_SECS=$((2 * 24 * 3600))
# facts.parquet is only rewritten when the SEC publishes new data (normally every weekday), so a failing
# facts download shows here even while the daily ticker snapshot keeps coming.
FACTS_MAX_AGE_SECS=$((7 * 24 * 3600))
PAPER_MAX_AGE_SECS=$((5 * 24 * 3600))
# A newly installed market data timer gets this long for its first sync (Tue-Sat 03:00, up to 3h).
MARKETDATA_FIRST_SYNC_SECS=$((4 * 24 * 3600))
# File that must exist at the root of a local BACKUP_DEST.
BACKUP_MARKER=.stocktrader-backup
# Default cap on files one backup may delete (normally a few compacted .jsonl per night); more aborts the
# run. Override with BACKUP_MAX_DELETE in the environment or .env, e.g. once after compacting a backlog.
DEFAULT_BACKUP_MAX_DELETE=50

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
            docker compose run --rm --no-deps -T collector sources </dev/null
            ;;
        logs)
            docker compose logs -f --tail=100 collector
            ;;
        stop)
            docker compose stop collector
            ;;
        sync-marketdata)
            run_sync marketdata "${@:2}"
            ;;
        sync-fundamentals)
            run_sync fundamentals "${@:2}"
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
        install-fundamentals-timer)
            install_fundamentals_timer "$self" "${2:-}"
            ;;
        paper-trade)
            paper_trade "${@:2}"
            ;;
        install-paper-timer)
            install_paper_timer "$self" "${2:-}"
            ;;
        check-symbols)
            prepare
            docker compose build paper >/dev/null
            docker compose run --rm --no-deps -T --entrypoint stocktrader paper symbols "${@:2}" </dev/null
            ;;
        *)
            sed -n '2,24p' "$self"
            return 1
            ;;
    esac
}

prepare() {
    # Created here, not by Docker, so they belong to this user rather than root. data/raw exists from the
    # start, so a missing archive root inside a container always means a misconfigured path.
    mkdir -p data/raw
    if [[ ! -e .env ]]; then
        install -m 600 .env.example .env
        echo "Created .env from .env.example. Starting with keyless sources only;"
        echo "add API keys to .env, then run: deploy/collector.sh up"
    fi
    chmod 600 .env
}

# Value of KEY from .env (last assignment wins), or empty. Surrounding quotes are stripped, as Compose does.
env_value() {
    sed -n "s/^[[:space:]]*$1=//p" .env 2>/dev/null | tail -n 1 | tr -d '\r' |
        sed -e 's/[[:space:]]*$//' -e 's/^"\(.*\)"$/\1/' -e "s/^'\(.*\)'$/\1/"
}

# A curl config line for URL. URLs go through a config on a pipe rather than argv, so other users on the
# host can't read webhook secrets from `ps`.
curl_url_config() {
    local v="${1//\\/\\\\}"
    printf 'url = "%s"\n' "${v//\"/\\\"}"
}

# Posts an alert to ALERT_WEBHOOK_URL (ntfy-style: plain-text body, Title header). Returns non-zero only
# if a configured webhook could not be reached, so callers can retry later.
notify() {
    local title="$1" body="$2" url
    url="$(env_value ALERT_WEBHOOK_URL)"
    if [[ -z "$url" ]]; then
        echo "ALERT_WEBHOOK_URL not set in .env; alert not sent: $title" >&2
        return 0
    fi
    if ! curl -fsS -m 20 -K <(curl_url_config "$url") -H "Title: $title" --data-binary @- \
        <<<"$body" >/dev/null; then
        echo "Failed to deliver alert: $title" >&2
        return 1
    fi
}

# Pings HEARTBEAT_URL (e.g. a healthchecks.io check) so something off this host notices if the host, its
# timers or this script stop running altogether.
heartbeat() {
    local url
    url="$(env_value HEARTBEAT_URL)"
    [[ -z "$url" ]] && return 0
    curl -fsS -m 20 -K <(curl_url_config "$url") -o /dev/null || echo "Heartbeat ping failed" >&2
}

# Runs `<service> sync ARGS...` (marketdata or fundamentals) as a one-shot container named
# stocktrader-<service>-sync. The fixed name means a sync that is still running (say, orphaned by a timeout)
# makes this one exit with ALREADY_RUNNING rather than overlap.
run_sync() {
    local service="$1"
    shift
    prepare
    SYNC_CONTAINER="stocktrader-$service-sync"
    if [[ -n "$(docker ps -q --filter "name=^${SYNC_CONTAINER}\$")" ]]; then
        echo "A $service sync is already running; not starting another." >&2
        return "$ALREADY_RUNNING"
    fi
    docker compose build "$service"
    # Clear only an exited leftover; `docker rm` without -f refuses a running container.
    docker rm "$SYNC_CONTAINER" >/dev/null 2>&1 || true
    docker compose run --rm -T --name "$SYNC_CONTAINER" "$service" sync "$@" </dev/null &
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
    heartbeat
    local output status=0 problems="" summary
    if [[ -z "$(docker compose ps -q --status running collector 2>/dev/null)" ]]; then
        problems+="collector NOT-RUNNING"$'\n'
    fi
    output="$(docker compose run --rm --no-deps -T collector health 2>&1 </dev/null)" || status=$?
    echo "$output"
    # Only the status table goes into alerts, never raw docker/compose output, which can quote .env lines.
    summary="$(grep -E '^[a-z_]+ +(ok|STALE|MISSING|DISABLED) ' <<<"$output" || true)"
    if ((status != 0)); then
        # Names and states only, so the alert key doesn't change as ages tick up.
        local found
        found="$(awk '$2 == "STALE" || $2 == "MISSING" || $2 == "DISABLED" { print $1 " " $2 }' <<<"$summary")"
        problems+="${found:-health-check FAILED}"$'\n'
        [[ -z "$found" ]] && summary+=$'\n'"health check could not run; see: journalctl --user -u stocktrader-health"
    fi
    local backup_line
    if [[ -n "$(env_value BACKUP_DEST)" ]]; then
        local last=0
        [[ -f data/.last-backup ]] && last="$(head -c 32 data/.last-backup | tr -dc '0-9')"
        [[ "$last" =~ ^[0-9]+$ ]] || last=0
        if (($(date +%s) - last > BACKUP_MAX_AGE_SECS)); then
            problems+="backup STALE"$'\n'
            backup_line="backup         STALE    last success $( ((last)) && date -d "@$last" -u +%FT%TZ || echo never)"
        else
            backup_line="backup         ok       last success $(date -d "@$last" -u +%FT%TZ)"
        fi
    else
        backup_line="backup         -        BACKUP_DEST not set in .env"
    fi
    echo "$backup_line"
    summary+=$'\n'"$backup_line"
    local job_line jobs=()
    # `collector health` reports market data once a sync has completed; until then, only this can notice
    # that a scheduled sync never succeeds.
    if [[ ! -e data/marketdata/manifest.json ]]; then
        jobs+=("$(job_status marketdata stocktrader-marketdata.timer data/marketdata/manifest.json "$MARKETDATA_FIRST_SYNC_SECS")")
    fi
    jobs+=("$(job_status fundamentals stocktrader-fundamentals.timer 'data/fundamentals/tickers/*.json' "$FUNDAMENTALS_MAX_AGE_SECS")")
    jobs+=("$(job_status facts stocktrader-fundamentals.timer data/fundamentals/facts.parquet "$FACTS_MAX_AGE_SECS")")
    jobs+=("$(job_status paper stocktrader-paper.timer 'data/live/journal/*.jsonl' "$PAPER_MAX_AGE_SECS")")
    for job_line in "${jobs[@]}"; do
        echo "$job_line"
        summary+=$'\n'"$job_line"
        if [[ "$job_line" =~ ^([a-z]+)\ +(STALE|MISSING)\  ]]; then
            problems+="${BASH_REMATCH[1]} ${BASH_REMATCH[2]}"$'\n'
        fi
    done
    problems="$(sed '/^[[:space:]]*$/d' <<<"$problems" | sort)"
    alert_on_change "$problems" "$summary"
    [[ -z "$problems" ]]
}

# One status-table line for a scheduled job: `-` until its timer is installed (and while a newly installed
# timer is younger than MAX_AGE seconds and nothing is written yet), then MISSING until it has written PATTERN
# (a glob, matched here), and STALE once the newest match is older than MAX_AGE.
job_status() {
    local name="$1" unit="$2" pattern="$3" max_age="$4" newest age state mtime unit_file
    unit_file="$HOME/.config/systemd/user/$unit"
    if [[ ! -e "$unit_file" ]]; then
        printf '%-14s %-8s %s\n' "$name" "-" "not scheduled ($unit not installed)"
        return 0
    fi
    # shellcheck disable=SC2086 # the pattern is a glob
    newest="$(ls -t $pattern 2>/dev/null | head -n 1)"
    if [[ -z "$newest" ]] || ! mtime="$(stat -c %Y "$newest" 2>/dev/null)"; then
        if (($(date +%s) - $(stat -c %Y "$unit_file") <= max_age)); then
            printf '%-14s %-8s %s\n' "$name" "-" "waiting for the first run ($unit installed recently)"
        else
            printf '%-14s %-8s %s\n' "$name" MISSING "nothing written yet ($pattern)"
        fi
        return 0
    fi
    age=$(($(date +%s) - mtime))
    state=ok
    ((age > max_age)) && state=STALE
    printf '%-14s %-8s %s\n' "$name" "$state" "last output $((age / 3600))h ago (limit $((max_age / 3600))h)"
}

# Alerts when the set of problems changes (including recovery), and re-alerts an unchanged problem daily.
# The new state is recorded only once the alert is delivered, so a failed send is retried next run.
alert_on_change() {
    local problems="$1" body="$2" state=data/.health-alert now last_time=0 last="" title
    now="$(date +%s)"
    if [[ -f "$state" ]]; then
        last_time="$(head -n 1 "$state" | tr -dc '0-9')"
        last="$(tail -n +2 "$state")"
    fi
    [[ "$last_time" =~ ^[0-9]+$ ]] || last_time=0
    if [[ "$problems" == "$last" ]] && { [[ -z "$problems" ]] || ((now - last_time < REALERT_SECS)); }; then
        return 0
    fi
    if [[ -z "$problems" ]]; then
        title="stocktrader on $(hostname): recovered"
    else
        title="stocktrader on $(hostname): $(tr '\n' ' ' <<<"$problems")"
    fi
    notify "$title" "$body" || return 0
    printf '%s\n%s\n' "$now" "$problems" >"$state.tmp" && mv "$state.tmp" "$state"
}

housekeeping() {
    prepare
    local failed=""
    docker compose run --rm --no-deps -T collector compact </dev/null || failed+="compaction "
    # Derived tables are rebuilt from the whole archive; a failure here never touches the archive. Build
    # first so a host whose image predates the signals binary doesn't fail every night.
    { docker compose build signals >/dev/null && docker compose run --rm --no-deps -T signals </dev/null; } ||
        failed+="signals "
    backup || failed+="backup "
    if [[ -n "$failed" ]]; then
        notify "stocktrader on $(hostname): ${failed}failed" "See: journalctl --user -u stocktrader-housekeeping"
        return 1
    fi
}

# Copies data/ to BACKUP_DEST (any rsync destination: local path, mounted drive, or user@host:path).
# Market data and fundamentals Parquet files and the SEC zip are skipped (they are re-downloadable), as are
# derived signal tables (rebuilt from the archive);
# everything else, including the raw archive, asset and ticker snapshots, and cursors, is copied.
#
# Deletion is tightly limited: only *.jsonl files can be deleted from the backup (`P! *.jsonl` protects
# everything else), so the uncompressed copy of a day goes once its .jsonl.gz has replaced it, but a
# compressed day is never removed even if it disappears locally. Deletes happen after all transfers
# (so the .gz lands before its .jsonl goes) and are capped by --max-delete.
#
# Every step checks its own result rather than relying on `set -e`, which bash ignores inside functions
# called from `||` (as housekeeping does). Success is recorded only after rsync succeeds.
backup() {
    local dest rc=0 max_delete
    dest="$(env_value BACKUP_DEST)"
    max_delete="${BACKUP_MAX_DELETE:-$(env_value BACKUP_MAX_DELETE)}"
    [[ "$max_delete" =~ ^[0-9]+$ ]] || max_delete=$DEFAULT_BACKUP_MAX_DELETE
    if [[ -z "$dest" ]]; then
        echo "BACKUP_DEST not set in .env; skipping backup."
        return 0
    fi
    if [[ -z "$(find data/raw -type f \( -name '*.jsonl' -o -name '*.jsonl.gz' \) -print -quit 2>/dev/null)" ]]; then
        echo "data/raw has no archive files; refusing to back up an empty archive." >&2
        return 1
    fi
    # A local destination must be marked, so an unmounted drive fails instead of filling the root disk.
    # Like rsync, treat it as remote only if a colon comes before the first slash (host:path).
    if [[ ! "$dest" =~ ^[^/]*: && ! -e "${dest%/}/$BACKUP_MARKER" ]]; then
        echo "BACKUP_DEST $dest has no $BACKUP_MARKER file (is the drive mounted?)." >&2
        echo "If this is the right place, create it once: touch '${dest%/}/$BACKUP_MARKER'" >&2
        return 1
    fi
    rsync -a --delete-after --max-delete="$max_delete" --filter='P! *.jsonl' \
        --exclude='*.tmp' --exclude='.sync.lock' --exclude='marketdata/*.parquet' \
        --exclude='fundamentals/*.parquet' --exclude='fundamentals/*.zip' --exclude='/signals/' \
        -e 'ssh -o BatchMode=yes' \
        data/ "${dest%/}/" || rc=$?
    # 24: some source files vanished during the transfer (normal for a live archive).
    if ((rc == 25)); then
        echo "Backup copied everything but needs to delete more than $max_delete compacted .jsonl files." >&2
        echo "Expected after compacting a backlog: run once with BACKUP_MAX_DELETE=1000 deploy/collector.sh backup" >&2
        echo "Unexpected otherwise: check data/raw for missing day files before raising the limit." >&2
        return "$rc"
    fi
    if ((rc != 0 && rc != 24)); then
        echo "rsync to $dest failed with exit $rc" >&2
        return "$rc"
    fi
    if ! { date +%s >data/.last-backup.tmp && mv data/.last-backup.tmp data/.last-backup; }; then
        echo "Backup succeeded but data/.last-backup could not be written" >&2
        return 1
    fi
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

# PATH escaped for a systemd unit's ExecStart: `%` starts a specifier and `$` a variable, so both double.
unit_path() {
    local p="${1//\%/%%}"
    printf '%s' "${p//\$/\$\$}"
}

install_ops() {
    local self units="${2:-$HOME/.config/systemd/user}"
    self="$(unit_path "$1")"
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

# Writes the service and timer for a sync job. Args: script units-dir name service-description
# timer-description subcommand calendar. Retries cover a catch-up run firing at boot before Docker is up,
# or a transient API outage; the start limit stops a persistent failure from retrying all day.
write_sync_units() {
    local self units="$2" name="$3" description="$4" timer_description="$5" subcommand="$6" calendar="$7"
    self="$(unit_path "$1")"
    mkdir -p "$units"
    cat >"$units/$name.service" <<EOF
[Unit]
Description=stocktrader: $description
StartLimitIntervalSec=12h
StartLimitBurst=3

[Service]
Type=oneshot
ExecStart="$self" $subcommand
# Another sync already running: nothing to do, and nothing to retry.
SuccessExitStatus=$ALREADY_RUNNING
# On timeout the script's SIGTERM trap stops the container it started.
TimeoutStartSec=3h
Restart=on-failure
RestartSec=20min
EOF
    cat >"$units/$name.timer" <<EOF
[Unit]
Description=stocktrader: $timer_description

[Timer]
OnCalendar=$calendar
RandomizedDelaySec=15m
Persistent=true

[Install]
WantedBy=timers.target
EOF
}

install_timer() {
    local units="${2:-$HOME/.config/systemd/user}"
    # After each US session (Mon-Fri), once Alpaca has the final daily bars.
    write_sync_units "$1" "$units" stocktrader-marketdata "download universe and daily price history" \
        "nightly market data sync" sync-marketdata "Tue..Sat 03:00 America/New_York"
    if [[ -n "${2:-}" ]]; then
        echo "Wrote units to $units without installing them."
        return 0
    fi
    enable_timers stocktrader-marketdata.timer
}

install_fundamentals_timer() {
    local units="${2:-$HOME/.config/systemd/user}"
    # The SEC regenerates its bulk files overnight; an unchanged file costs one conditional request.
    write_sync_units "$1" "$units" stocktrader-fundamentals "SEC company fundamentals" \
        "daily SEC fundamentals sync" sync-fundamentals "*-*-* 06:00 America/New_York"
    if [[ -n "${2:-}" ]]; then
        echo "Wrote units to $units without installing them."
        return 0
    fi
    enable_timers stocktrader-fundamentals.timer
}

# Runs `stocktrader paper` with PAPER_ARGS from .env (strategy and symbols) plus ARGS. A failed run that was
# meant to submit orders raises an alert.
paper_trade() {
    prepare
    local args=() status=0
    read -r -a args <<<"$(env_value PAPER_ARGS)"
    if ((${#args[@]} == 0)); then
        echo "PAPER_ARGS is not set in .env; for example: PAPER_ARGS=--symbols AAPL,MSFT,NVDA --strategy sma" >&2
        status=1
    else
        docker compose build paper >/dev/null || status=$?
    fi
    local output="" report copy
    if ((status == 0)); then
        # Streamed (so a run killed by a timeout still leaves its output in the journal) and copied for the report.
        copy="$(mktemp)"
        docker compose run --rm --no-deps -T paper "${args[@]}" "$@" 2>&1 </dev/null | tee "$copy" || status=$?
        output="$(cat "$copy")"
        rm -f "$copy"
    fi
    [[ " ${args[*]} $* " == *" --submit "* ]] || return "$status"
    # Only the runner's own report lines go into alerts, never raw docker/compose output, which can quote
    # .env lines.
    report="$(grep -E '^(session|equity|open orders|kill switch|rejected|left alone|order|no orders|submitted|warning:|Error:|Caused by:|    [0-9]+: )' \
        <<<"$output" || true)"
    if ((status != 0)); then
        local body="See: journalctl --user -u stocktrader-paper"
        [[ -n "$report" ]] && body="$report"$'\n\n'"$body"
        notify "stocktrader on $(hostname): paper trading failed" "$body" || true
    elif ! grep -q 'nothing to do' <<<"$output"; then
        local sent
        sent="$(grep '^submitted' <<<"$report" | grep -vc 'already submitted' || true)"
        notify "stocktrader paper on $(hostname): ${sent} order(s) sent" "$report" || true
    fi
    return "$status"
}

install_paper_timer() {
    local self units="${2:-$HOME/.config/systemd/user}"
    self="$(unit_path "$1")"
    mkdir -p "$units"
    # Weekdays before the open, after the nightly market data sync. The run itself skips market holidays and
    # refuses stale data. Not Persistent: a run caught up after a late boot would trade mid-session.
    write_timer_units "$units" stocktrader-paper "paper trading before the open" \
        "\"$self\" paper-trade --submit" 20min "Mon..Fri 08:45 America/New_York" "RandomizedDelaySec=5m"
    if [[ -n "${2:-}" ]]; then
        echo "Wrote units to $units without installing them."
        return 0
    fi
    [[ -z "$(env_value PAPER_ARGS)" ]] && echo "Warning: PAPER_ARGS is not set in .env; runs will fail until it is."
    enable_timers stocktrader-paper.timer
}

main "$@"
exit
