#!/usr/bin/env bash
# install.sh — curl-pipe installer for ma-demo (the Migration Assistant demo
# environment harness).
#
# Detects your OS + architecture, downloads the matching release binary from
# GitHub, and installs it onto your PATH. No build toolchain required.
#
# Usage:
#   curl -fsSL https://github.com/AndreKurait/opensearch-migrations-demo/releases/latest/download/install.sh | bash
#
# Env overrides:
#   MA_DEMO_VERSION   pin a release tag (default: latest)
#   MA_DEMO_REPO      owner/repo override (default below)
#   BIN_DIR           install dir (default: ~/.local/bin)

set -o errexit
set -o nounset
set -o pipefail

REPO="${MA_DEMO_REPO:-AndreKurait/opensearch-migrations-demo}"
VERSION="${MA_DEMO_VERSION:-latest}"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
BIN_NAME="ma-demo"

c_red()   { printf '\033[31m%s\033[0m' "$1"; }
c_green() { printf '\033[32m%s\033[0m' "$1"; }
c_yellow(){ printf '\033[33m%s\033[0m' "$1"; }
c_dim()   { printf '\033[2m%s\033[0m' "$1"; }
c_bold()  { printf '\033[1m%s\033[0m' "$1"; }

die() { printf '%s %s\n' "$(c_red 'error:')" "$*" >&2; exit 1; }
require() { command -v "$1" >/dev/null 2>&1 || die "required command not on PATH: $1"; }

require uname
require tar

# --- detect target triple ---
detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Linux)  os="unknown-linux-gnu" ;;
    Darwin) os="apple-darwin" ;;
    *) die "unsupported OS: $os (linux/macos only)" ;;
  esac
  case "$arch" in
    x86_64|amd64)  arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) die "unsupported architecture: $arch" ;;
  esac
  printf '%s-%s' "$arch" "$os"
}

# --- resolve the release version (latest → the published tag) ---
resolve_version() {
  if [[ "$VERSION" != "latest" ]]; then
    printf '%s\n' "$VERSION"
    return
  fi
  require curl
  local tag
  tag=$(curl -fsSL --max-time 10 \
    "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep -o '"tag_name":[[:space:]]*"[^"]*"' \
    | head -1 \
    | sed -E 's/.*"([^"]+)"$/\1/' || true)
  [[ -z "$tag" ]] && die "could not resolve latest release from https://github.com/${REPO}/releases"
  printf '%s\n' "$tag"
}

main() {
  require curl
  printf 'Installing %s…\n\n' "$(c_bold ma-demo)"

  local target version stripped tarball url tmp
  target="$(detect_target)"
  version="$(resolve_version)"
  # Asset names embed the version without the leading "v".
  stripped="${version#v}"
  tarball="ma-demo-${stripped}-${target}.tar.gz"
  url="https://github.com/${REPO}/releases/download/${version}/${tarball}"

  printf '  %s %s\n' "$(c_dim 'target:')" "$target"
  printf '  %s %s\n\n' "$(c_dim 'version:')" "$version"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL --max-time 120 -o "$tmp/$tarball" "$url" \
    || die "could not download $url"
  tar -xzf "$tmp/$tarball" -C "$tmp"

  # The tarball contains a single binary named ma-demo-<target>; install it as
  # `ma-demo`.
  local extracted="$tmp/${BIN_NAME}-${target}"
  [[ -f "$extracted" ]] || extracted="$(find "$tmp" -type f -name "${BIN_NAME}*" ! -name '*.tar.gz' | head -1)"
  [[ -f "$extracted" ]] || die "release tarball did not contain a ma-demo binary"

  mkdir -p "$BIN_DIR"
  install -m 0755 "$extracted" "$BIN_DIR/$BIN_NAME"

  printf '%s %s %s installed → %s\n\n' \
    "$(c_green '✔')" "$BIN_NAME" "$version" "$BIN_DIR/$BIN_NAME"

  if ! printf '%s' "$PATH" | tr ':' '\n' | grep -qxF "$BIN_DIR"; then
    printf '%s %s is not on your PATH. Add it to your shell rc:\n' \
      "$(c_yellow '!')" "$BIN_DIR"
    # shellcheck disable=SC2016
    printf '    export PATH="%s:$PATH"\n\n' "$BIN_DIR"
  fi

  printf '%s\n' "$(c_bold 'Next: run the wizard')"
  printf '    %s\n\n' "$(c_bold "$BIN_NAME")"
  printf '  It will stand up a test environment for the OpenSearch Migration\n'
  printf '  Assistant, then open a live status dashboard. Needs docker, kind,\n'
  printf '  kubectl, curl on PATH (helm for the local MA deploy).\n'
  printf '%s Installation complete!\n' "$(c_green '✓')"
}

main "$@"
