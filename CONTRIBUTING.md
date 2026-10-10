# Contributing to Kagantic Voice Recorder

Kagantic Voice Recorder is a single Rust package with two binaries — `kvr`
(terminal) and `kvr-gui` (desktop) — built on a shared library core
(`src/lib.rs`, `ogg.rs`, `encoder.rs`, `audio.rs`, `devices.rs`,
`session.rs`). The same code builds the Linux and Windows release
artifacts.

## Prerequisites

- **Stable Rust** — use the toolchain selected by
  [`rust-toolchain.toml`](rust-toolchain.toml): `channel = "stable"`
  tracks the current stable release, not a pinned version.
- **A C compiler and `cmake`** — the bundled libopus (`opusic-c`) is built
  from source as part of the build. No system `libopus-dev`, `libogg`, or
  `ffmpeg` *library* is needed to build or run.
- **Platform packages** (see the [README](README.md#building-from-source)):
  - Debian/Ubuntu: `pkg-config`, `libasound2-dev`, `libgtk-3-dev`, `cmake`
  - Fedora: `pkgconf-pkg-config`, `alsa-lib-devel`, `gtk3-devel`, `cmake`
  - Windows: Visual Studio Build Tools with *Desktop development with C++*
    (MSVC + Windows SDK) and CMake on `PATH`
- **`ffmpeg` and `ffprobe` on `PATH`** — required by the integration tests
  and for verifying recordings. The tests are the only thing that needs
  them; the app itself does not.
- **Python 3** — packaging-time PE inspection and its fixture tests (not an app runtime dependency).
- **No microphone or display required** for the test suite (see
  [Tests](#tests)).

## Development loop

```bash
cargo build --locked --release
cargo run --locked --bin kvr -- start          # terminal recorder, REPL: p/r/s/q
cargo run --locked --bin kvr-gui               # desktop workspace (needs a display)
```

The CLI requires the `start` subcommand to record; `pause`, `resume`,
`stop`, `status`, and `where` operate on the active session (Unix external
controls via signals).

## Quality gates

The repository is gated on `rustfmt`, `clippy`, and the test suite — the
same gates that
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs on every push
and pull request, which also performs a full `cargo build --locked
--release` on both native platforms. Run them locally before pushing:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python3 scripts/check-windows-pe.py --self-test
```

The three Rust gates are supplemented by the portable PE fixture check in
both CI workflows. There is no additional formatter or pre-commit hook;
keep code `rustfmt`-clean and warning-free.

## Tests

All test audio is **synthesized** (single tones or separate stereo tones), so the suite is safe,
deterministic, and needs no microphone, audio server, or window display:

| Test | What it checks |
|---|---|
| `tests/end_to_end.rs` | synthesized sine → encoder → Ogg mux; `ffprobe` reports `codec_name=opus`; `ffmpeg` decodes to WAV without error |
| `tests/wav.rs` | PCM WAV round-trip: `--format wav` writes a valid RIFF `fmt ` file; `ffprobe` reports `codec_name=pcm_s16le` (or `pcm_f32le` per negotiated rate) and the same sample count comes back through a re-encode |
| `tests/raw.rs` | raw `f32` little-endian round-trip: `--format raw` writes headerless samples; every frame survives a bit-exact read-back through `python3 -c 'import sys, struct; …'` |
| `tests/no_gaps.rs` | strictly increasing granule positions under simulated CPU contention |
| `tests/no_gaps_under_jitter.rs` | no dropped or duplicated frames when the pipeline stalls |
| `tests/recording_pipeline.rs` | production drain/sinks at 44.1 kHz stereo with irregular callbacks and partial tail; live/final byte counts equal file length; ffmpeg verifies decoded frame count, nonzero channel-specific tones, bit-exact raw; ffprobe validates rate/channels/duration; Ogg framing, CRC, EOS and granule checked; existing output never truncated |
| unit tests in `src/audio.rs` / `src/output.rs` | supported 44.1 kHz configurations, unsupported/hinted channel errors, callback/error diagnostics, continuous-phase channel-safe resampling |
| unit tests in `src/lib.rs` / `src/writers.rs` | CLI destination resolution/collisions preserve existing takes, native directory respected, headerless empty WAV and classic RIFF limit boundary without allocating a 4 GiB file |
| `scripts/check-windows-pe.py --self-test` | non-executable header/resource fixtures accept GUI Subsystem 2 / CLI Subsystem 3 and reject swapped subsystems, wrong architecture, malformed headers, missing/wrong icon groups or pixels and out-of-bounds resource directories |
| unit tests in `src/bin/gui.rs` | mocked production workers verify live/final size and ffmpeg decoded nonzero samples/duration; absent callbacks/valid silence, late backend failure while paused and valid partial WAV; writable output fallback and GUI saved/error states; format/extension precedence; multi-frame egui keyboard editing and paused level history |
| unit tests in `src/session.rs` (Windows) | real-process liveness: own PID, running child, and completed child; the helper child is always killed and reaped |

Practical notes:

- Integration tests shell out to `ffprobe` / `ffmpeg`; both must be on `PATH`.
- Integration tests use unique temporary directories under the operating
  system's temporary-file location and remove them automatically. No
  `/tmp` or `C:\tmp` setup is required.
- Device-picker unit tests run without opening any capture device; use them
  when touching `devices.rs` — no hardware is required.

## Verifying a real recording

```bash
ffprobe -v error -select_streams a:0 -show_entries stream=codec_name,sample_rate,channels recording.opus
ffmpeg -i recording.opus -f wav out.wav
```

## Style and scope

- Follow existing patterns; the codebase has one convention for error
  handling (`anyhow` in binaries, `thiserror` for library errors), one for
  Ogg page writing (`ogg.rs`), and one for control flow (`Control`
  messages). Do not introduce a second mechanism alongside them.
- Keep `Cargo.lock` in sync with manifest changes; it is tracked in git and
  the release packaging relies on it.
- If you add a test that depends on an external tool, a directory, or
  platform behavior, document it in this file and in the test's header
  comment.

## Releases (context for contributors)

- `.github/workflows/ci.yml` — `rustfmt` check, `clippy`, tests, and a
  release build on both native platforms, on every push and pull request.
- `.github/workflows/release.yml` — automatic versioned releases from `main`,
  pushed version tags, and manual (artifact-only) builds:
  - A push to `main` publishes the Cargo version if its GitHub release does
    not exist. Already-published versions skip the release jobs.
  - Versions come from Cargo metadata and must be stable `X.Y.Z` values.
    Pushed tags must match `vX.Y.Z`.
  - After **both** platform test/build/package jobs pass, publication creates
    the version tag at the tested commit if absent, then uploads standalone
    applications, archives and checksums. An existing tag pointing elsewhere is rejected.
  - Publication is serialized per version and never overwrites release assets.
    If publication fails after creating the tag, rerun that workflow commit;
    bump the version before publishing different code.
  - Manual dispatch builds the artifacts only — it never creates a
    release, even if a tag is selected in the dispatch UI.
  - Build/test/package jobs get read-only repository access; only the
    publish job has `contents:write`.
- Packaging scripts have a fixed interface:

  ```bash
  bash scripts/package-release.sh v0.1.3 --bin-dir target/release --output-dir dist
  bash scripts/package-appimage.sh v0.1.3 --bin-dir target/release --output-dir dist
  pwsh scripts/package-release.ps1 v0.1.3 -BinDir target/release -OutputDir dist
  ```

  They enforce the version match, the single-root archive layout, the
  binary smoke test, and checksum verification, and never overwrite an
  existing archive. Windows packaging requires Python 3 to inspect both original
  and extracted ZIP binaries: x86-64 PE32+, GUI Subsystem 2, CLI Subsystem 3.
  It also rejects Visual C++ runtime DLL imports in both sets, using Visual
  Studio's `dumpbin` on Windows or binutils' `objdump` on Linux. Native packaging
  smoke-tests the extracted CLI; cross-packaging explicitly skips runtime execution.
  ZIP entries are written explicitly with forward-slash paths, including on
  Windows PowerShell 5.1; `Compress-Archive` can otherwise emit backslash names
  that violate the canonical archive-layout check.
  Windows packaging also emits direct version-named CLI/GUI EXEs with sidecars.
  The PE checker validates ICON/GROUP_ICON frames against the original-logo ICO;
  native Windows additionally compares shell-extracted icon pixels to the PNG.
  AppImage packaging uses pinned linuxdeploy/GTK tooling, bundles native dialog
  dependencies, schemas, icons and notices, and validates the extracted image.
  It requires Pillow (`python3-pil` on Ubuntu). `scripts/check-linux-gui.py`
  runs the actual AppImage under Xvfb/Mesa and checks `WM_CLASS` and `_NET_WM_ICON`
  pixels; CI needs `xvfb`, `x11-utils`, `imagemagick` and `libgl1-mesa-dri`.
  Keep `.cargo/config.toml` and `.cargo/msvc-runtime.cmake` enabled: Rust and
  bundled libopus must both use the static MSVC runtime. Runner smoke tests alone
  cannot detect dynamic-runtime regressions because the runner has the redistributable.
  CI workflow edits do not establish that a Windows job passed; use an authorized
  non-publishing CI/artifact build and the README's native acceptance checklist.
