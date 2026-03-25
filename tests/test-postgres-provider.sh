#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/tests/docker-compose.yml"
KEEP_DB="${KEEP_DB:-0}"
TEST_URL="${TEST_URL:-postgres://postgres:demo@127.0.0.1:5432/postgres}"
WAIT_SECONDS="${WAIT_SECONDS:-90}"

cleanup() {
  if [[ "${KEEP_DB}" == "1" ]]; then
    echo "[wp-knowledge] KEEP_DB=1, skip docker compose down"
    return
  fi
  docker compose -f "${COMPOSE_FILE}" down -v
}

trap cleanup EXIT

cd "${ROOT_DIR}"

if [[ -n "${WP_KDB_TEST_POSTGRES_URL:-}" ]]; then
  echo "[wp-knowledge] ignore inherited WP_KDB_TEST_POSTGRES_URL=${WP_KDB_TEST_POSTGRES_URL}"
  echo "[wp-knowledge] use TEST_URL=${TEST_URL} for this compose-managed test run"
fi

docker compose -f "${COMPOSE_FILE}" up -d

CONTAINER_ID="$(docker compose -f "${COMPOSE_FILE}" ps -q postgres)"
if [[ -z "${CONTAINER_ID}" ]]; then
  echo "[wp-knowledge] postgres container id not found" >&2
  exit 1
fi

for ((i = 0; i < WAIT_SECONDS; i++)); do
  STATUS="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "${CONTAINER_ID}" 2>/dev/null || true)"
  if [[ "${STATUS}" == "healthy" || "${STATUS}" == "running" ]]; then
    if docker compose -f "${COMPOSE_FILE}" exec -T postgres pg_isready -U postgres -d postgres >/dev/null 2>&1; then
      break
    fi
  fi
  if [[ "${STATUS}" == "exited" || "${STATUS}" == "dead" ]]; then
    echo "[wp-knowledge] postgres container stopped unexpectedly, status=${STATUS}" >&2
    docker compose -f "${COMPOSE_FILE}" logs postgres >&2 || true
    exit 1
  fi
  sleep 1
done

if ! docker compose -f "${COMPOSE_FILE}" exec -T postgres pg_isready -U postgres -d postgres >/dev/null 2>&1; then
  echo "[wp-knowledge] postgres is not ready after ${WAIT_SECONDS}s" >&2
  docker compose -f "${COMPOSE_FILE}" logs postgres >&2 || true
  STATUS="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "${CONTAINER_ID}" 2>/dev/null || true)"
  echo "[wp-knowledge] final container status: ${STATUS:-unknown}" >&2
  exit 1
fi

for _ in {1..5}; do
  if docker compose -f "${COMPOSE_FILE}" exec -T postgres psql -U postgres -d postgres -c 'select 1' >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

if ! docker compose -f "${COMPOSE_FILE}" exec -T postgres psql -U postgres -d postgres -c 'select 1' >/dev/null 2>&1; then
  echo "[wp-knowledge] postgres is accepting connections but simple SQL probe still fails" >&2
  docker compose -f "${COMPOSE_FILE}" logs postgres >&2 || true
  exit 1
fi

export WP_KDB_TEST_POSTGRES_URL="${TEST_URL}"

echo "[wp-knowledge] running postgres_provider with ${WP_KDB_TEST_POSTGRES_URL}"
cargo test --test postgres_provider -- --ignored --nocapture
