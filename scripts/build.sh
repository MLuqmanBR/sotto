#!/usr/bin/env bash
# Build Sotto in the only order that works from a clean tree.
#
# The Tauri configuration bundles `target/release/sotto-stt` as a resource. Tauri
# checks that resource while running its own build script, which happens before
# the rest of the workspace has finished linking — so on a tree that has never
# been built, `cargo build --workspace --release` fails with
# "resource path ../target/release/sotto-stt doesn't exist".
#
# Build the sidecar first, then the app. Every other build entry point uses this
# script rather than repeating the order.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build -p sotto-stt --release
cargo build --workspace --release