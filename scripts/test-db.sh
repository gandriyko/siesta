#!/usr/bin/env bash
# Starts Postgres, MariaDB, Elasticsearch and MongoDB in Docker and runs the db-exec and siesta-cli integration tests against them.
# Containers stay running afterwards; `scripts/test-db.sh --down` stops and removes them.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${1:-}" == "--down" ]]; then
    exec docker compose down -v
fi

docker compose up -d --wait
# A second database per server for the per-file database tests (idempotent: containers outlive runs).
docker compose exec -T postgres psql -U siesta -d siesta -tAc "SELECT 1 FROM pg_database WHERE datname = 'siesta_b'" | grep -q 1 \
    || docker compose exec -T postgres psql -U siesta -d siesta -c "CREATE DATABASE siesta_b"
docker compose exec -T mariadb mariadb -uroot -psiesta -e "CREATE DATABASE IF NOT EXISTS siesta_b; GRANT ALL ON siesta_b.* TO 'siesta'@'%'"
export SIESTA_TEST_PG_URL="postgres://siesta:siesta@127.0.0.1:25432/siesta"
export SIESTA_TEST_MYSQL_URL="mysql://siesta:siesta@127.0.0.1:23306/siesta"
export SIESTA_TEST_ES_URL="http://elastic:siesta@127.0.0.1:29200"
export SIESTA_TEST_MONGO_URL="mongodb://siesta:siesta@127.0.0.1:27018/siesta?authSource=admin"
cargo test -p db-exec -p siesta-cli -- --include-ignored "$@"
