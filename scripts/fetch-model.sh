#!/usr/bin/env bash
# Fetch the speech model into ./model for bundling into the package.
set -euo pipefail
cd "$(dirname "$0")/.."
BASE="https://huggingface.co/moondream/parakeet-redux/resolve/main"
mkdir -p model
for f in config.json model.safetensors ternary.json tokenizer.json; do
  if [ -f "model/$f" ]; then echo "have $f"; continue; fi
  echo "fetching $f"
  curl -fL --retry 3 -o "model/$f" "$BASE/$f"
done
echo "model ready: $(du -sh model | cut -f1)"
