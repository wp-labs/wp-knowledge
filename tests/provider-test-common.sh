#!/usr/bin/env bash

compose() {
  docker compose -p "${COMPOSE_PROJECT_NAME}" -f "${COMPOSE_FILE}" "$@"
}

cleanup_compose() {
  if [[ "${KEEP_DB}" == "1" ]]; then
    echo "[wp-knowledge] KEEP_DB=1, skip docker compose down"
    return
  fi
  compose down -v
}

install_compose_cleanup_trap() {
  trap cleanup_compose EXIT
}

log_inherited_test_url() {
  local inherited_var="$1"
  if [[ -n "${!inherited_var:-}" ]]; then
    echo "[wp-knowledge] ignore inherited ${inherited_var}=${!inherited_var}"
    echo "[wp-knowledge] use TEST_URL=${TEST_URL} for this compose-managed test run"
  fi
}

start_service() {
  local service="$1"
  compose up -d "${service}"
}

container_id_for() {
  local service="$1"
  compose ps -q "${service}"
}

wait_for_container_ready() {
  local service="$1"
  local container_id="$2"
  local wait_seconds="$3"

  for ((i = 0; i < wait_seconds; i++)); do
    local status
    status="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "${container_id}" 2>/dev/null || true)"
    if [[ "${status}" == "healthy" || "${status}" == "running" ]]; then
      return 0
    fi
    if [[ "${status}" == "exited" || "${status}" == "dead" ]]; then
      echo "[wp-knowledge] ${service} container stopped unexpectedly, status=${status}" >&2
      compose logs "${service}" >&2 || true
      return 1
    fi
    sleep 1
  done

  echo "[wp-knowledge] ${service} did not reach running state after ${wait_seconds}s" >&2
  compose logs "${service}" >&2 || true
  return 1
}

wait_for_exec_success() {
  local service="$1"
  local wait_seconds="$2"
  shift 2

  for ((i = 0; i < wait_seconds; i++)); do
    if compose exec -T "${service}" "$@" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done

  return 1
}

require_exec_success() {
  local service="$1"
  local failure_message="$2"
  shift 2

  if ! compose exec -T "${service}" "$@" >/dev/null 2>&1; then
    echo "${failure_message}" >&2
    compose logs "${service}" >&2 || true
    return 1
  fi
}
