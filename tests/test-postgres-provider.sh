#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

"${ROOT_DIR}/tests/test-postgres-provider-correctness.sh"
"${ROOT_DIR}/tests/test-postgres-provider-perf.sh"
