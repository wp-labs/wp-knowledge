#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/tests/docker-compose.yml"
KEEP_DB="${KEEP_DB:-0}"
TEST_URL="${TEST_URL:-mysql://root:demo@127.0.0.1:3306/demo}"
WAIT_SECONDS="${WAIT_SECONDS:-120}"
export WP_KDB_PERF_ROWS="${WP_KDB_PERF_ROWS:-10000}"
export WP_KDB_PERF_OPS="${WP_KDB_PERF_OPS:-10000}"
export WP_KDB_PERF_HOTSET="${WP_KDB_PERF_HOTSET:-128}"
MYSQL_TESTS=(
  mysql_provider_query_and_pool
  mysql_provider_cache_perf
  mysql_provider_sync_vs_async_perf
  mysql_provider_async_cache_concurrency_perf
)

cleanup() {
  if [[ "${KEEP_DB}" == "1" ]]; then
    echo "[wp-knowledge] KEEP_DB=1, skip docker compose down"
    return
  fi
  docker compose -f "${COMPOSE_FILE}" down -v
}

trap cleanup EXIT

cd "${ROOT_DIR}"

if [[ -n "${WP_KDB_TEST_MYSQL_URL:-}" ]]; then
  echo "[wp-knowledge] ignore inherited WP_KDB_TEST_MYSQL_URL=${WP_KDB_TEST_MYSQL_URL}"
  echo "[wp-knowledge] use TEST_URL=${TEST_URL} for this compose-managed test run"
fi

docker compose -f "${COMPOSE_FILE}" up -d mysql

CONTAINER_ID="$(docker compose -f "${COMPOSE_FILE}" ps -q mysql)"
if [[ -z "${CONTAINER_ID}" ]]; then
  echo "[wp-knowledge] mysql container id not found" >&2
  exit 1
fi

for ((i = 0; i < WAIT_SECONDS; i++)); do
  STATUS="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "${CONTAINER_ID}" 2>/dev/null || true)"
  if [[ "${STATUS}" == "healthy" || "${STATUS}" == "running" ]]; then
    if docker compose -f "${COMPOSE_FILE}" exec -T mysql mysqladmin ping -h 127.0.0.1 -uroot -pdemo >/dev/null 2>&1; then
      break
    fi
  fi
  if [[ "${STATUS}" == "exited" || "${STATUS}" == "dead" ]]; then
    echo "[wp-knowledge] mysql container stopped unexpectedly, status=${STATUS}" >&2
    docker compose -f "${COMPOSE_FILE}" logs mysql >&2 || true
    exit 1
  fi
  sleep 1
done

if ! docker compose -f "${COMPOSE_FILE}" exec -T mysql mysqladmin ping -h 127.0.0.1 -uroot -pdemo >/dev/null 2>&1; then
  echo "[wp-knowledge] mysql is not ready after ${WAIT_SECONDS}s" >&2
  docker compose -f "${COMPOSE_FILE}" logs mysql >&2 || true
  STATUS="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "${CONTAINER_ID}" 2>/dev/null || true)"
  echo "[wp-knowledge] final container status: ${STATUS:-unknown}" >&2
  exit 1
fi

for _ in {1..5}; do
  if docker compose -f "${COMPOSE_FILE}" exec -T mysql mysql -uroot -pdemo -D demo -e 'select 1' >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

if ! docker compose -f "${COMPOSE_FILE}" exec -T mysql mysql -uroot -pdemo -D demo -e 'select 1' >/dev/null 2>&1; then
  echo "[wp-knowledge] mysql is accepting connections but simple SQL probe still fails" >&2
  docker compose -f "${COMPOSE_FILE}" logs mysql >&2 || true
  exit 1
fi

export WP_KDB_TEST_MYSQL_URL="${TEST_URL}"

echo "[wp-knowledge] running mysql_provider with ${WP_KDB_TEST_MYSQL_URL} (perf rows=${WP_KDB_PERF_ROWS} ops=${WP_KDB_PERF_OPS} hotset=${WP_KDB_PERF_HOTSET})"
for test_name in "${MYSQL_TESTS[@]}"; do
  echo "[wp-knowledge] running mysql_provider::${test_name}"
  cargo test --test mysql_provider "${test_name}" -- --ignored --nocapture
done
