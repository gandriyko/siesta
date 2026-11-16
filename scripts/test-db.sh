#!/usr/bin/env bash
# Starts Postgres and MariaDB in Docker and runs the db-exec integration tests against them.
# Containers stay running afterwards; `scripts/test-db.sh --down` stops and removes them.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${1:-}" == "--down" ]]; then
    exec docker compose down -v
fi

docker compose up -d --wait
export SIESTA_TEST_PG_URL="postgres://siesta:siesta@127.0.0.1:25432/siesta"
export SIESTA_TEST_MYSQL_URL="mysql://siesta:siesta@127.0.0.1:23306/siesta"
cargo test -p db-exec -- --include-ignored "$@"
