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
```

There is no additional formatter, pre-commit hook, or linter gate beyond
these three; keep code `rustfmt`-clean and warning-free.

## Tests

All test audio is **synthesized** (a 1 kHz sine wave), so the suite is safe,
deterministic, and needs no microphone, audio server, or window display:

| Test | What it checks |
|---|---|
| `tests/end_to_end.rs` | synthesized sine → encoder → Ogg mux; `ffprobe` reports `codec_name=opus`; `ffmpeg` decodes to WAV without error |
| `tests/wav.rs` | PCM WAV round-trip: `--format wav` writes a valid RIFF `fmt ` file; `ffprobe` reports `codec_name=pcm_s16le` (or `pcm_f32le` per negotiated rate) and the same sample count comes back through a re-encode |
| `tests/raw.rs` | raw `f32` little-endian round-trip: `--format raw` writes headerless samples; every frame survives a bit-exact read-back through `python3 -c 'import sys, struct; …'` |
| `tests/no_gaps.rs` | strictly increasing granule positions under simulated CPU contention |
| `tests/no_gaps_under_jitter.rs` | no dropped or duplicated frames when the pipeline stalls |
| unit tests in `src/bin/gui.rs` | paused level-history freezes and resumes without a gap (headless `egui` context — no display needed) |
| unit tests in `src/session.rs` (Windows) | real-process liveness: own PID, running child, and completed child; the helper child is always killed and reaped |

Practical notes:

- The three integration tests shell out to `ffprobe` (and `ffmpeg` for
  `end_to_end.rs`) — both must be on `PATH`.
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
    the version tag at the tested commit if absent, then uploads the release
    archives and checksums. An existing tag pointing elsewhere is rejected.
  - Publication is serialized per version and never overwrites release assets.
    If publication fails after creating the tag, rerun that workflow commit;
    bump the version before publishing different code.
  - Manual dispatch builds the artifacts only — it never creates a
    release, even if a tag is selected in the dispatch UI.
  - Build/test/package jobs get read-only repository access; only the
    publish job has `contents:write`.
- Packaging scripts have a fixed interface:

  ```bash
  bash scripts/package-release.sh v0.1.0 --bin-dir target/release --output-dir dist
  pwsh scripts/package-release.ps1 v0.1.0 -BinDir target/release -OutputDir dist
  ```

  They enforce the version match, the single-root archive layout, the
  binary smoke test, and checksum verification, and never overwrite an
  existing archive. Windows packaging additionally rejects Visual C++ runtime
  DLL imports in both binaries, using Visual Studio's `dumpbin` on Windows or
  binutils' `objdump` on Linux. Keep `.cargo/config.toml` and
  `.cargo/msvc-runtime.cmake` enabled: Rust and bundled libopus must both use
  the static MSVC runtime. Runner smoke tests alone cannot detect this
  regression because the runner already has the redistributable installed.
