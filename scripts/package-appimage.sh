#!/usr/bin/env bash
# Package the GUI with GTK dependencies and original-logo desktop metadata.
# Usage: bash scripts/package-appimage.sh v0.1.3 --bin-dir target/release --output-dir dist
set -euo pipefail

fail() { echo "ERROR: $*" >&2; exit 1; }
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION=""
BIN_DIR="$ROOT_DIR/target/release"
OUTPUT_DIR="$ROOT_DIR/dist"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --bin-dir|--output-dir)
      option="$1"; shift
      [ "$#" -gt 0 ] || fail "$option requires a directory"
      if [ "$option" = --bin-dir ]; then BIN_DIR="$1"; else OUTPUT_DIR="$1"; fi
      ;;
    --help|-h)
      echo 'Usage: package-appimage.sh <VERSION> [--bin-dir DIR] [--output-dir DIR]'; exit 0 ;;
    -*) fail "unknown option: $1" ;;
    *) [ -z "$VERSION" ] || fail "unexpected argument: $1"; VERSION="${1#v}" ;;
  esac
  shift
done
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail 'VERSION must be X.Y.Z or vX.Y.Z'
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || fail 'Linux x86_64 required'
case "$BIN_DIR" in /*) ;; *) BIN_DIR="$ROOT_DIR/$BIN_DIR" ;; esac
case "$OUTPUT_DIR" in /*) ;; *) OUTPUT_DIR="$ROOT_DIR/$OUTPUT_DIR" ;; esac
for command in cargo python3 curl sha256sum file; do
  command -v "$command" >/dev/null || fail "required command not found: $command"
done
python3 -c 'from PIL import Image' || fail 'Pillow is required (Ubuntu: python3-pil)'
CARGO_VERSION="$(cd "$ROOT_DIR" && cargo metadata --locked --no-deps --format-version 1 | python3 -c '
import json, os, sys
meta = json.load(sys.stdin)
root = (meta.get("resolve") or {}).get("root")
packages = meta.get("packages", [])
pkg = next((p for p in packages if p.get("id") == root), None) if root else None
if pkg is None:
    manifest = os.path.abspath("Cargo.toml")
    pkg = next((p for p in packages if os.path.abspath(p.get("manifest_path", "")) == manifest), None)
if pkg is None:
    sys.exit("root package not found in cargo metadata")
print(pkg["version"])
')"
[ "$CARGO_VERSION" = "$VERSION" ] || fail "version $VERSION does not match Cargo $CARGO_VERSION"
for binary in kvr kvr-gui; do
  [ -x "$BIN_DIR/$binary" ] || fail "missing executable: $BIN_DIR/$binary"
  file "$BIN_DIR/$binary" | grep -q 'ELF.*x86-64' || fail "$binary must be ELF x86_64"
done
[ "$("$BIN_DIR/kvr" --version)" = "kvr $VERSION" ] || fail 'stale or mismatched binaries'
NAME="kvr-gui-v${VERSION}-linux-x86_64.AppImage"
mkdir -p "$OUTPUT_DIR"
for output in "$OUTPUT_DIR/$NAME" "$OUTPUT_DIR/$NAME.sha256"; do
  [ ! -e "$output" ] && [ ! -L "$output" ] || fail "output already exists (not overwriting): $output"
done
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT
TOOLS="$STAGING/tools"
APPDIR="$STAGING/AppDir"
mkdir -p "$TOOLS" "$APPDIR/assets/fonts"
for notice in LICENSE THIRD_PARTY_NOTICES.md README.md assets/pixel-art-logo.png assets/fonts/Silkscreen-OFL.txt assets/fonts/VT323-OFL.txt assets/fonts/DotGothic16-OFL.txt; do
  cp "$ROOT_DIR/$notice" "$APPDIR/$notice"
done
DESKTOP="$STAGING/com.github.kaganerkan.KaganticVoiceRecorder.desktop"
cat > "$DESKTOP" <<'EOF'
[Desktop Entry]
Type=Application
Name=Kagantic Voice Recorder
Comment=Record microphone audio locally
Exec=kvr-gui
Icon=kagantic-voice-recorder
Terminal=false
Categories=AudioVideo;Audio;Recorder;
StartupWMClass=com.github.kaganerkan.KaganticVoiceRecorder
EOF
python3 - "$ROOT_DIR/assets/pixel-art-logo.png" "$APPDIR" <<'PY'
from pathlib import Path
import sys
from PIL import Image
logo = Image.open(sys.argv[1]).convert('RGBA')
# Ubuntu 22.04 ships Pillow 9.0, before the Resampling enum was added.
nearest = getattr(Image, 'Resampling', Image).NEAREST
root = Path(sys.argv[2])
for size in (32, 64, 128, 256):
    destination = root / f'usr/share/icons/hicolor/{size}x{size}/apps/kagantic-voice-recorder.png'
    destination.parent.mkdir(parents=True, exist_ok=True)
    logo.resize((size, size), nearest).save(destination)
logo.resize((256, 256), nearest).save(root / 'kagantic-voice-recorder.png')
(root / '.DirIcon').symlink_to('kagantic-voice-recorder.png')
PY

# Pinned official release and immutable GTK plugin revision, checked before execution.
curl -fL --retry 2 'https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20250213-2/linuxdeploy-x86_64.AppImage' -o "$TOOLS/linuxdeploy.AppImage"
curl -fL --retry 2 'https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/7a3fbc31a9e5075073ff8790f26effbac5f84453/linuxdeploy-plugin-gtk.sh' -o "$TOOLS/linuxdeploy-plugin-gtk.sh"
printf '%s  %s\n' '4648f278ab3ef31f819e67c30d50f462640e5365a77637d7e6f2ad9fd0b4522a' "$TOOLS/linuxdeploy.AppImage" 'b0f4cbc684a0103a9651f0955b635eaea0096b3a66c0f5a2c2aa337960375171' "$TOOLS/linuxdeploy-plugin-gtk.sh" | sha256sum -c
chmod +x "$TOOLS/linuxdeploy.AppImage" "$TOOLS/linuxdeploy-plugin-gtk.sh"
mkdir "$TOOLS/extracted"
(cd "$TOOLS/extracted" && "$TOOLS/linuxdeploy.AppImage" --appimage-extract >/dev/null)
LINUXDEPLOY="$TOOLS/extracted/squashfs-root/AppRun"
export LINUXDEPLOY
export DEPLOY_GTK_VERSION=3
export APPIMAGE_EXTRACT_AND_RUN=1
export PATH="$TOOLS:$PATH"
export ARCH=x86_64
export VERSION
OUTPUT="$STAGING/$NAME"
export OUTPUT
(cd "$STAGING" && "$LINUXDEPLOY" --appdir "$APPDIR" \
  --executable "$BIN_DIR/kvr-gui" --executable "$BIN_DIR/kvr" \
  --desktop-file "$DESKTOP" --icon-file "$APPDIR/kagantic-voice-recorder.png" \
  --plugin gtk)
python3 "$ROOT_DIR/scripts/collect-appimage-notices.py" "$APPDIR"
(cd "$STAGING" && "$LINUXDEPLOY" --appdir "$APPDIR" --output appimage)
[ -x "$OUTPUT" ] || fail 'linuxdeploy did not produce the requested AppImage'
mkdir "$STAGING/verify"
(cd "$STAGING/verify" && "$OUTPUT" --appimage-extract >/dev/null)
python3 - "$STAGING/verify/squashfs-root" "$ROOT_DIR/assets/pixel-art-logo.png" <<'PY'
from pathlib import Path
import configparser, sys
import hashlib, json
from PIL import Image
root = Path(sys.argv[1])
original = Image.open(sys.argv[2]).convert('RGBA')
desktop = root / 'com.github.kaganerkan.KaganticVoiceRecorder.desktop'
metadata = configparser.ConfigParser(interpolation=None)
metadata.read(desktop)
entry = metadata['Desktop Entry']
assert entry['Exec'] == 'kvr-gui' and entry['Icon'] == 'kagantic-voice-recorder'
assert entry['StartupWMClass'] == 'com.github.kaganerkan.KaganticVoiceRecorder'
assert entry['Terminal'] == 'false'
assert (root / 'AppRun').is_file() and (root / 'usr/bin/kvr-gui').is_file()
nearest = getattr(Image, 'Resampling', Image).NEAREST
for path in [root / '.DirIcon', root / 'kagantic-voice-recorder.png'] + [
        root / f'usr/share/icons/hicolor/{s}x{s}/apps/kagantic-voice-recorder.png' for s in (32, 64, 128, 256)]:
    image = Image.open(path).convert('RGBA')
    assert image.size in [(s, s) for s in (32, 64, 128, 256)], (path, image.size)
    assert image.tobytes() == original.resize(image.size, nearest).tobytes(), path
assert list(root.glob('usr/lib/libgtk-3.so*')), 'GTK3 not bundled'
assert list(root.glob('usr/share/glib-2.0/schemas/gschemas.compiled')), 'GLib schemas not bundled'
for notice in ('LICENSE', 'THIRD_PARTY_NOTICES.md', 'assets/fonts/Silkscreen-OFL.txt', 'assets/fonts/VT323-OFL.txt', 'assets/fonts/DotGothic16-OFL.txt'):
    assert (root / notice).is_file(), notice
notices = root / 'usr/share/doc/kagantic-voice-recorder/libraries'
provenance = json.loads((notices / 'provenance.json').read_text())
for library in provenance['libraries']:
    assert hashlib.sha256((root / library['path']).read_bytes()).hexdigest() == library['sha256']
if provenance['distribution'] == 'Freedesktop SDK':
    assert (notices / 'freedesktop-sdk-manifest.json').is_file()
    assert (notices / 'freedesktop-sdk-licenses/freedesktop-sdk/gtk3/COPYING').is_file()
else:
    assert (notices / 'common-licenses/LGPL-2.1').is_file()
    for package in provenance['packages']:
        assert (notices / (package['binary'] + '.copyright')).is_file()
        assert package['source_url']
print('Extracted AppImage desktop identity, original-logo pixels, GTK3, schemas and notices verified')
PY
# noclobber opens exclusively; a concurrent packaging run cannot overwrite assets.
(set -o noclobber; cat "$OUTPUT" > "$OUTPUT_DIR/$NAME")
chmod +x "$OUTPUT_DIR/$NAME"
HASH="$(sha256sum "$OUTPUT_DIR/$NAME")"
HASH="${HASH%% *}"
(set -o noclobber; printf '%s  %s\n' "$HASH" "$NAME" > "$OUTPUT_DIR/$NAME.sha256")
(cd "$OUTPUT_DIR" && sha256sum -c "$NAME.sha256")
echo "wrote $OUTPUT_DIR/$NAME"
