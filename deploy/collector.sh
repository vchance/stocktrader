#!/usr/bin/env bash
# Manage the collector container on a Linux host with Docker. Run from anywhere.
#
#   deploy/collector.sh up       build from the current checkout and (re)start
#   deploy/collector.sh update   git pull --ff-only, then up
#   deploy/collector.sh status   container state and enabled sources
#   deploy/collector.sh logs     follow logs
#   deploy/collector.sh stop     stop (data and .env are kept)
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
            docker compose logs -f --tail=100
            ;;
        stop)
            docker compose stop
            ;;
        *)
            sed -n '2,8p' "$self"
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

main "$@"
exit
