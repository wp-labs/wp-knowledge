#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "${ROOT_DIR}"

export WP_KDB_PERF_ROWS="${WP_KDB_PERF_ROWS:-10000}"
export WP_KDB_PERF_OPS="${WP_KDB_PERF_OPS:-120000}"
export WP_KDB_PERF_HOTSET="${WP_KDB_PERF_HOTSET:-128}"

echo "[wp-knowledge] cache perf rows=${WP_KDB_PERF_ROWS} ops=${WP_KDB_PERF_OPS} hotset=${WP_KDB_PERF_HOTSET}"
RUSTC_WRAPPER= cargo test cache_perf_reports_cache_vs_no_cache -- --ignored --nocapture
