#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/tests/docker-compose.yml"
KEEP_DB="${KEEP_DB:-0}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-wpkdb-mysql-correctness}"
TEST_URL="${TEST_URL:-mysql://root:demo@127.0.0.1:3306/demo}"
WAIT_SECONDS="${WAIT_SECONDS:-120}"
MYSQL_TESTS=(
  mysql_provider_reconnects_after_connection_kill
  mysql_provider_query_and_pool
  mysql_provider_sqlx_type_compatibility
)

. "${ROOT_DIR}/tests/provider-test-common.sh"
install_compose_cleanup_trap

cd "${ROOT_DIR}"

log_inherited_test_url "WP_KDB_TEST_MYSQL_URL"

start_service mysql

CONTAINER_ID="$(container_id_for mysql)"
if [[ -z "${CONTAINER_ID}" ]]; then
  echo "[wp-knowledge] mysql container id not found" >&2
  exit 1
fi

wait_for_container_ready mysql "${CONTAINER_ID}" "${WAIT_SECONDS}"

if ! wait_for_exec_success mysql "${WAIT_SECONDS}" mysqladmin ping -h 127.0.0.1 -uroot -pdemo; then
  echo "[wp-knowledge] mysql is not ready after ${WAIT_SECONDS}s" >&2
  compose logs mysql >&2 || true
  STATUS="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "${CONTAINER_ID}" 2>/dev/null || true)"
  echo "[wp-knowledge] final container status: ${STATUS:-unknown}" >&2
  exit 1
fi

if ! wait_for_exec_success mysql 5 mysql -uroot -pdemo -D demo -e 'select 1'; then
  require_exec_success mysql "[wp-knowledge] mysql is accepting connections but simple SQL probe still fails" \
    mysql -uroot -pdemo -D demo -e 'select 1'
  exit 1
fi

export WP_KDB_TEST_MYSQL_URL="${TEST_URL}"

echo "[wp-knowledge] running mysql correctness tests with ${WP_KDB_TEST_MYSQL_URL} (compose project=${COMPOSE_PROJECT_NAME})"
for test_name in "${MYSQL_TESTS[@]}"; do
  echo "[wp-knowledge] running mysql_provider::${test_name}"
  cargo test --test mysql_provider "${test_name}" -- --ignored --nocapture
done
