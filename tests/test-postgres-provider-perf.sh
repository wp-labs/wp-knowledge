#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/tests/docker-compose.yml"
KEEP_DB="${KEEP_DB:-0}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-wpkdb-postgres-perf}"
TEST_URL="${TEST_URL:-postgres://postgres:demo@127.0.0.1:5432/postgres}"
WAIT_SECONDS="${WAIT_SECONDS:-90}"
export WP_KDB_PERF_ROWS="${WP_KDB_PERF_ROWS:-10000}"
export WP_KDB_PERF_OPS="${WP_KDB_PERF_OPS:-10000}"
export WP_KDB_PERF_HOTSET="${WP_KDB_PERF_HOTSET:-128}"
POSTGRES_TESTS=(
  postgres_provider_cache_perf
  postgres_provider_sync_vs_async_perf
  postgres_provider_async_cache_concurrency_perf
)

. "${ROOT_DIR}/tests/provider-test-common.sh"
install_compose_cleanup_trap

cd "${ROOT_DIR}"

log_inherited_test_url "WP_KDB_TEST_POSTGRES_URL"

start_service postgres

CONTAINER_ID="$(container_id_for postgres)"
if [[ -z "${CONTAINER_ID}" ]]; then
  echo "[wp-knowledge] postgres container id not found" >&2
  exit 1
fi

wait_for_container_ready postgres "${CONTAINER_ID}" "${WAIT_SECONDS}"

if ! wait_for_exec_success postgres "${WAIT_SECONDS}" pg_isready -U postgres -d postgres; then
  echo "[wp-knowledge] postgres is not ready after ${WAIT_SECONDS}s" >&2
  compose logs postgres >&2 || true
  STATUS="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "${CONTAINER_ID}" 2>/dev/null || true)"
  echo "[wp-knowledge] final container status: ${STATUS:-unknown}" >&2
  exit 1
fi

if ! wait_for_exec_success postgres 5 psql -U postgres -d postgres -c 'select 1'; then
  require_exec_success postgres "[wp-knowledge] postgres is accepting connections but simple SQL probe still fails" \
    psql -U postgres -d postgres -c 'select 1'
  exit 1
fi

export WP_KDB_TEST_POSTGRES_URL="${TEST_URL}"

echo "[wp-knowledge] running postgres perf tests with ${WP_KDB_TEST_POSTGRES_URL} (compose project=${COMPOSE_PROJECT_NAME} perf rows=${WP_KDB_PERF_ROWS} ops=${WP_KDB_PERF_OPS} hotset=${WP_KDB_PERF_HOTSET})"
for test_name in "${POSTGRES_TESTS[@]}"; do
  echo "[wp-knowledge] running postgres_provider::${test_name}"
  cargo test --test postgres_provider "${test_name}" -- --ignored --nocapture
done
