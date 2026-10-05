#!/usr/bin/env bash
#
# Package kvr into a Linux x86_64 release archive from prebuilt
# binaries. This script supports exactly one native platform: Linux/x86_64.
# Any other host platform or architecture is rejected rather than
# mislabelled.
#
# The archive contains exactly, under a single root directory named
# `kvr-v<VERSION>-linux-x86_64`:
#
#   kvr, kvr-gui                    the two release binaries (ELF x86-64)
#   README.md
#   LICENSE
#   THIRD_PARTY_NOTICES.md
#   assets/fonts/Silkscreen-OFL.txt
#   assets/fonts/VT323-OFL.txt
#   assets/fonts/DotGothic16-OFL.txt
#   assets/pixel-art-logo.png       required, so packaged README links resolve
#
# A `<archive>.sha256` sidecar in sha256sum format is written next to the
# archive and verified with `sha256sum -c` before the script exits.
#
# Usage (relative paths are resolved against the repository root):
#
#   bash scripts/package-release.sh v0.1.0 --bin-dir target/release --output-dir dist
#
# The version (with or without a leading `v`) must equal the Cargo package
# version. Binaries are validated as ELF x86-64 before packaging, and the
# CLI is smoke-tested (`kvr --version` must report exactly
# `kvr <VERSION>`). The
# script never succeeds with missing or mismatched inputs and never
# overwrites an existing archive or sidecar.

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: package-release.sh <VERSION> [--bin-dir DIR] [--output-dir DIR]

  VERSION         Release version with an optional `v` prefix (for example
                  `v0.1.0`). Must equal the Cargo package version.
  --bin-dir DIR   Directory containing the release binaries
                  (default: target/release).
  --output-dir DIR
                  Directory to create the archive in (default: dist).

Packages exactly one Linux x86_64 archive:
  kvr-v<VERSION>-linux-x86_64.tar.gz plus a .sha256 sidecar.
EOF
}

fail() {
  echo "error: $*" >&2
  exit 1
}

TAG=""
BIN_DIR="target/release"
OUTPUT_DIR="dist"

while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir)
      if [ $# -lt 2 ]; then
        echo "error: --bin-dir requires a value" >&2
        usage >&2
        exit 2
      fi
      BIN_DIR="$2"
      shift 2
      ;;
    --output-dir)
      if [ $# -lt 2 ]; then
        echo "error: --output-dir requires a value" >&2
        usage >&2
        exit 2
      fi
      OUTPUT_DIR="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      echo "error: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      if [ -n "$TAG" ]; then
        echo "error: expected exactly one version (got: $TAG and $1)" >&2
        exit 2
      fi
      TAG="$1"
      shift
      ;;
  esac
done

if [ -z "$TAG" ]; then
  usage >&2
  exit 2
fi

if [[ "$TAG" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
  VERSION="${BASH_REMATCH[1]}"
elif [[ "$TAG" =~ ^([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
  VERSION="${BASH_REMATCH[1]}"
else
  fail "VERSION must be X.Y.Z or vX.Y.Z (got: $TAG)"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Relative paths are always resolved against the repository root so the
# script behaves identically from any working directory.
case "$BIN_DIR" in
  /*) ;;
  *) BIN_DIR="$ROOT_DIR/$BIN_DIR" ;;
esac
case "$OUTPUT_DIR" in
  /*) ;;
  *) OUTPUT_DIR="$ROOT_DIR/$OUTPUT_DIR" ;;
esac

# Exactly one native platform: Linux x86_64.
OS_RAW="$(uname -s 2>/dev/null || echo unknown)"
ARCH_RAW="$(uname -m 2>/dev/null || echo unknown)"
if [ "$OS_RAW" != "Linux" ]; then
  fail "this script packages Linux builds only; unsupported platform: $OS_RAW (use scripts/package-release.ps1 for Windows)"
fi
if [ "$ARCH_RAW" != "x86_64" ]; then
  fail "this script packages x86_64 builds only; unsupported architecture: $ARCH_RAW"
fi

command -v cargo >/dev/null 2>&1 || fail "cargo is not on PATH"
command -v python3 >/dev/null 2>&1 || fail "python3 is not on PATH"
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is not on PATH"
command -v tar >/dev/null 2>&1 || fail "tar is not on PATH"

# Resolve the root package version from the cargo metadata JSON. The root
# package is selected via resolve.root, falling back to the manifest path.
if ! CARGO_VERSION="$(
  cd "$ROOT_DIR" || exit 1
  cargo metadata --locked --no-deps --format-version 1 2>/dev/null |
    python3 -c '
import json
import os
import sys

with sys.stdin:
    meta = json.load(sys.stdin)

def find(pred):
    for pkg in meta.get("packages", []):
        if pred(pkg):
            return pkg
    return None

root_id = (meta.get("resolve") or {}).get("root")
pkg = find(lambda p: p.get("id") == root_id) if root_id else None
if pkg is None:
    target = os.path.abspath(os.path.join(os.getcwd(), "Cargo.toml"))
    pkg = find(lambda p: os.path.abspath(p.get("manifest_path", "")) == target)
if pkg is None:
    sys.exit("root package not found in cargo metadata")
sys.stdout.write(pkg["version"] + "\n")
'
)"; then
  fail "could not determine the Cargo package version from $ROOT_DIR"
fi
if [ -z "$CARGO_VERSION" ]; then
  fail "could not determine the Cargo package version from $ROOT_DIR"
fi
if [ "$CARGO_VERSION" != "$VERSION" ]; then
  fail "version $VERSION does not match the Cargo package version $CARGO_VERSION"
fi

# Reject binaries that are not genuine ELF x86-64 executables.
require_elf_x86_64() {
  local f="$1"
  local magic machine
  magic="$(od -An -N4 -t x1 "$f" | tr -d '[:space:]')"
  [ "$magic" = "7f454c46" ] || fail "$f is not an ELF executable"
  machine="$(od -An -j18 -N2 --endian=little -t u2 "$f" | tr -d '[:space:]')"
  [ "$machine" = "62" ] || fail "$f is not x86-64 (e_machine=$machine, expected 62)"
}

for BIN in kvr kvr-gui; do
  [ -f "$BIN_DIR/$BIN" ] || fail "missing release binary: $BIN_DIR/$BIN"
  [ -x "$BIN_DIR/$BIN" ] || fail "release binary is not executable: $BIN_DIR/$BIN"
  require_elf_x86_64 "$BIN_DIR/$BIN"
done

# Smoke the CLI: --version must succeed and report the exact CLI string.
CLI_VERSION_OUT="$("$BIN_DIR/kvr" --version 2>&1)" || fail "kvr --version failed"
if [ "$CLI_VERSION_OUT" != "kvr $VERSION" ]; then
  fail "kvr --version did not report the exact string 'kvr $VERSION': $CLI_VERSION_OUT"
fi

ROOT_NAME="kvr-v${VERSION}-linux-x86_64"
STAGING_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGING_DIR"' EXIT
DEST_ROOT="$STAGING_DIR/$ROOT_NAME"
mkdir -p "$DEST_ROOT/assets/fonts"

copy_repo_file() {
  local src_rel="$1"
  local src="$ROOT_DIR/$src_rel"
  [ -f "$src" ] || fail "required file not found: $src_rel"
  local dest="$DEST_ROOT/$src_rel"
  mkdir -p "$(dirname "$dest")"
  cp -f "$src" "$dest"
}

for BIN in kvr kvr-gui; do
  cp -f "$BIN_DIR/$BIN" "$DEST_ROOT/$BIN"
done

copy_repo_file "README.md"
copy_repo_file "LICENSE"
copy_repo_file "THIRD_PARTY_NOTICES.md"
copy_repo_file "assets/fonts/Silkscreen-OFL.txt"
copy_repo_file "assets/fonts/VT323-OFL.txt"
copy_repo_file "assets/fonts/DotGothic16-OFL.txt"
copy_repo_file "assets/pixel-art-logo.png"

mkdir -p "$OUTPUT_DIR"
ARCHIVE_NAME="${ROOT_NAME}.tar.gz"
ARCHIVE_PATH="$OUTPUT_DIR/$ARCHIVE_NAME"
SIDECAR_PATH="${ARCHIVE_PATH}.sha256"
if [ -e "$ARCHIVE_PATH" ]; then
  fail "output already exists (not overwriting): $ARCHIVE_PATH"
fi
if [ -e "$SIDECAR_PATH" ]; then
  fail "output already exists (not overwriting): $SIDECAR_PATH"
fi

tar -czf "$ARCHIVE_PATH" -C "$STAGING_DIR" "$ROOT_NAME"

# Enforce the single-root-directory layout.
BAD_ENTRY="$(tar -tzf "$ARCHIVE_PATH" | grep -v "^${ROOT_NAME}/" | head -n1 || true)"
if [ -n "$BAD_ENTRY" ]; then
  fail "archive contains entries outside the root directory: $BAD_ENTRY"
fi

HASH="$(sha256sum "$ARCHIVE_PATH" | awk '{print $1}')"
case "$HASH" in
  ""|*[!a-f0-9]*)
    fail "unexpected SHA-256 output: $HASH"
    ;;
esac
printf '%s  %s\n' "$HASH" "$ARCHIVE_NAME" > "$SIDECAR_PATH"
(cd "$OUTPUT_DIR" && sha256sum -c --quiet "${ARCHIVE_NAME}.sha256") \
  || fail "checksum verification failed for $ARCHIVE_NAME"

echo "wrote $ARCHIVE_PATH"
echo "wrote $SIDECAR_PATH"
