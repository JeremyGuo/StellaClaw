#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ZGENT_ROOT="$REPO_ROOT/zgent"
ZGENT_MANIFEST="$ZGENT_ROOT/Cargo.toml"
TARGET_DIR="$(cargo metadata --manifest-path "$ZGENT_MANIFEST" --no-deps --format-version 1 | jq -r '.target_directory')"

usage() {
  cat <<'EOF'
Usage: scripts/build_zgent_server.sh [--skip-install]

Install the system dependencies needed to build ./zgent and then compile
zgent-server into ./zgent/target/{debug,release}.

Options:
  --skip-install   Skip OS package installation and only run cargo build.
EOF
}

SKIP_INSTALL=0
if [[ $# -gt 1 ]]; then
  usage >&2
  exit 1
fi
if [[ $# -eq 1 ]]; then
  case "$1" in
    --skip-install)
      SKIP_INSTALL=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 1
      ;;
  esac
fi

if [[ ! -f "$ZGENT_MANIFEST" ]]; then
  echo "Expected zgent source tree at $ZGENT_ROOT" >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required to build zgent-server." >&2
  exit 1
fi

install_debian_deps() {
  local packages=(
    build-essential
    pkg-config
    libssl-dev
  )
  sudo apt-get update
  sudo apt-get install -y "${packages[@]}"
}

maybe_install_build_deps() {
  if [[ "$SKIP_INSTALL" -eq 1 ]]; then
    return 0
  fi

  if [[ -r /etc/os-release ]]; then
    # shellcheck disable=SC1091
    source /etc/os-release
    case "${ID:-}" in
      ubuntu|debian)
        install_debian_deps
        return 0
        ;;
    esac
    case "${ID_LIKE:-}" in
      *debian*)
        install_debian_deps
        return 0
        ;;
    esac
  fi

  echo "Unsupported OS for automatic dependency install. Re-run with --skip-install after installing pkg-config and OpenSSL development headers manually." >&2
  exit 1
}

maybe_install_build_deps

echo "Building zgent-server from $ZGENT_ROOT"
cargo build --manifest-path "$ZGENT_MANIFEST" --bin zgent-server

if [[ -x "$TARGET_DIR/debug/zgent-server" ]]; then
  echo "Built: $TARGET_DIR/debug/zgent-server"
elif [[ -x "$TARGET_DIR/release/zgent-server" ]]; then
  echo "Built: $TARGET_DIR/release/zgent-server"
else
  echo "cargo build completed but zgent-server binary was not found under $TARGET_DIR" >&2
  exit 1
fi
