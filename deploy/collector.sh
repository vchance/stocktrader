#!/usr/bin/env bash
# Manage the collector container on a Linux host with Docker. Run from anywhere inside the repo checkout.
#
#   deploy/collector.sh up       build from the current checkout and (re)start
#   deploy/collector.sh update   git pull --ff-only, then up
#   deploy/collector.sh status   container state and enabled sources
#   deploy/collector.sh logs     follow logs
#   deploy/collector.sh stop     stop (data and .env are kept)
set -euo pipefail

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
export COLLECTOR_UID COLLECTOR_GID
COLLECTOR_UID="$(id -u)"
COLLECTOR_GID="$(id -g)"

prepare() {
    # Created here, not by Docker, so they belong to this user rather than root.
    mkdir -p data
    if [[ ! -e .env ]]; then
        cp .env.example .env
        echo "Created .env from .env.example; add API keys there, then run: $0 up"
    fi
    chmod 600 .env
}

case "${1:-}" in
    up)
        prepare
        docker compose up -d --build
        docker compose ps
        ;;
    update)
        git pull --ff-only
        "$0" up
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
        sed -n '2,9p' "$0"
        exit 1
        ;;
esac
