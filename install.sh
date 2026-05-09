#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
BIN_NAME="choros"

command -v cargo >/dev/null 2>&1 || {
  echo "error: cargo not found. install Rust: https://rustup.rs" >&2
  exit 1
}
command -v git >/dev/null 2>&1 || {
  echo "error: git not found. install git first." >&2
  exit 1
}

echo "==> building (cargo build --release)"
cargo build --release

mkdir -p "$INSTALL_DIR"
install -m 0755 "target/release/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
echo "==> installed: $INSTALL_DIR/$BIN_NAME"

case ":$PATH:" in
  *":$INSTALL_DIR:"*)
    ;;
  *)
    echo
    echo "note: $INSTALL_DIR is not on your PATH."
    case "$(basename "${SHELL:-}")" in
      zsh)
        echo "  add to ~/.zshrc:"
        echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
      bash)
        echo "  add to ~/.bashrc (or ~/.bash_profile on macOS):"
        echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
      fish)
        echo "  run once:"
        echo "    fish_add_path $INSTALL_DIR"
        ;;
      *)
        echo "  add this to your shell rc:"
        echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
    esac
    ;;
esac

# Shell-integration nudge: enables `choros work` to cd into the new choros.
already_sourced=0
for rc in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile"; do
  [[ -f "$rc" ]] && grep -q 'choros shell-init' "$rc" && already_sourced=1
done
if [[ "$already_sourced" -eq 0 ]]; then
  echo
  echo "tip: enable \`choros work\` cd-into behavior — add to your shell rc:"
  echo "    eval \"\$(choros shell-init)\""
fi
