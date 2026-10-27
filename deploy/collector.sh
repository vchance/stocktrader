#!/usr/bin/env bash
# Manage stocktrader's data services on a Linux host with Docker. Run from anywhere.
#
#   deploy/collector.sh up               build from the current checkout and (re)start the collector
#   deploy/collector.sh update           git pull --ff-only, then up
#   deploy/collector.sh status           container state and enabled sources
#   deploy/collector.sh logs             follow collector logs
#   deploy/collector.sh stop             stop the collector (data and .env are kept)
#   deploy/collector.sh sync-marketdata  download the universe and full daily price history now
#   deploy/collector.sh install-timer    run sync-marketdata nightly via a systemd user timer
set -euo pipefail

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
            prepare
            docker compose build marketdata
            docker compose run --rm marketdata
            ;;
        install-timer)
            install_timer "$self"
            ;;
        *)
            sed -n '2,11p' "$self"
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

install_timer() {
    local self="$1" units="$HOME/.config/systemd/user"
    mkdir -p "$units"
    cat > "$units/stocktrader-marketdata.service" <<EOF
[Unit]
Description=stocktrader: download universe and daily price history

[Service]
Type=oneshot
ExecStart=$self sync-marketdata
TimeoutStartSec=3h
EOF
    # After each US session (Mon-Fri), once Alpaca has the final daily bars.
    cat > "$units/stocktrader-marketdata.timer" <<EOF
[Unit]
Description=stocktrader: nightly market data sync

[Timer]
OnCalendar=Tue..Sat 03:00 America/New_York
RandomizedDelaySec=15m
Persistent=true

[Install]
WantedBy=timers.target
EOF
    systemctl --user daemon-reload
    systemctl --user enable --now stocktrader-marketdata.timer
    systemctl --user list-timers stocktrader-marketdata.timer --no-pager
    if [[ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != "yes" ]]; then
        echo "Warning: lingering is off, so the timer only runs while $USER is logged in."
        echo "Enable it with: sudo loginctl enable-linger $USER"
    fi
}

main "$@"
exit
