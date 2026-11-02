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

# Module-scoped temp dir; the EXIT trap (set once, here) cleans it whether we
# succeed or die. Declared before main() so it's in scope under `set -u`.
tmp=""
staged=""
trap '[[ -z "$tmp" ]] || rm -rf "$tmp"; [[ -z "$staged" ]] || rm -f "$staged"' EXIT

require uname
require tar

# --- sha256 of a file, as a bare lowercase hex digest (portable) ---
# Prefers shasum (macOS), falls back to sha256sum (Linux). Verification is
# mandatory: a missing tool must never turn a verified install into an unchecked one.
sha256_of() {
  local f="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$f" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$f" | awk '{print $1}'
  else
    die "required checksum tool not on PATH: shasum or sha256sum"
  fi
}

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
  command -v shasum >/dev/null 2>&1 || command -v sha256sum >/dev/null 2>&1 \
    || die "required checksum tool not on PATH: shasum or sha256sum"
  printf 'Installing %s…\n\n' "$(c_bold ma-demo)"

  # `tmp` is intentionally NOT local — it's the module global the EXIT trap
  # cleans (the trap fires after main returns, when a local would be gone).
  local target version stripped tarball url
  target="$(detect_target)"
  version="$(resolve_version)"
  # Asset names embed the version without the leading "v".
  stripped="${version#v}"
  tarball="ma-demo-${stripped}-${target}.tar.gz"
  url="https://github.com/${REPO}/releases/download/${version}/${tarball}"

  printf '  %s %s\n' "$(c_dim 'target:')" "$target"
  printf '  %s %s\n\n' "$(c_dim 'version:')" "$version"

  # A module-scoped temp dir cleaned by the EXIT trap (declared global so the
  # trap, which fires after main() returns, can still see it under `set -u`).
  tmp="$(mktemp -d)"
  curl -fsSL --max-time 120 -o "$tmp/$tarball" "$url" \
    || die "could not download $url"

  # Verify the published SHA-256 before trusting the archive (supply-chain
  # integrity). Every release ships a "<tarball>.sha256" alongside the tarball.
  # The local digest tool is the same shasum/sha256sum the release used.
  local local_sum expected_sum checksum_line checksum_name extra
  local_sum="$(sha256_of "$tmp/$tarball")"
  checksum_line="$(curl -fsSL --max-time 30 "${url}.sha256")" \
    || die "could not download required checksum ${tarball}.sha256"
  [[ "$checksum_line" != *$'\n'* ]] || die "checksum must contain exactly one entry"
  read -r expected_sum checksum_name extra <<< "$checksum_line"
  [[ "$expected_sum" =~ ^[0-9a-f]{64}$ && "$checksum_name" == "$tarball" && -z "$extra" ]] \
    || die "invalid checksum entry for $tarball"
  if [[ "$local_sum" != "$expected_sum" ]]; then
    die "checksum mismatch for $tarball
    expected: $expected_sum
    actual:   $local_sum
  The download may be corrupt or tampered with — aborting."
  fi
  printf '  %s %s\n' "$(c_dim 'sha256:')" "$(c_green 'verified')"

  # Releases contain exactly one target-specific binary. Reject other layouts
  # instead of guessing. Extract to stdout so archive paths and links cannot
  # write anywhere on disk.
  local member="${BIN_NAME}-${target}" entries extracted="$tmp/binary"
  entries="$(tar -tzf "$tmp/$tarball")" || die "could not read release archive"
  [[ "$entries" == "$member" ]] || die "release archive must contain only $member"
  tar -xOzf "$tmp/$tarball" "$member" > "$extracted" \
    || die "could not extract $member"
  [[ -s "$extracted" ]] || die "release binary is empty or is not a regular file"

  mkdir -p "$BIN_DIR"
  staged="$(mktemp "$BIN_DIR/.ma-demo.XXXXXX")"
  install -m 0755 "$extracted" "$staged"
  mv -f "$staged" "$BIN_DIR/$BIN_NAME"
  staged=""

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
