#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
target="$INSTALL_DIR/wspace"

if [[ -f "$target" ]]; then
  rm -f "$target"
  echo "removed $target"
else
  echo "not found: $target"
fi
