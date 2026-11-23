#!/usr/bin/env bash
# Run Sotto from the build tree.
set -euo pipefail
cd "$(dirname "$0")/.."
export SOTTO_STT_BIN="$PWD/target/release/sotto-stt"
export SOTTO_MODEL_DIR="$PWD/model"
exec ./target/release/sotto "$@"
