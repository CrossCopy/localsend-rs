#!/bin/sh
# Container e2e for localsend-rs. Requires Docker.
set -eu
cd "$(dirname "$0")"
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
docker compose up --build --abort-on-container-exit --exit-code-from sender
status=$?
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
exit $status
