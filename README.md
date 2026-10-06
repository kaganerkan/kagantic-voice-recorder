<p align="center">
  <img src="assets/pixel-art-logo.png" alt="Kagantic Voice Recorder pixel-art logo" width="128">
</p>

<h1 align="center">Kagantic Voice Recorder</h1>

<p align="center">
  A native Rust microphone recorder that writes standard Opus audio
  (Ogg container, RFC 7845). Fully local: no network, no cloud, no
  telemetry. Two front ends, one pipeline:
  <strong><code>kvr</code></strong> — terminal CLI with an interactive REPL —
  and <strong><code>kvr-gui</code></strong> — a native desktop workspace
  (<code>eframe</code>/<code>egui</code>, OpenGL rendering). Both write
  ordinary <code>.opus</code> files that play in VLC, Firefox, and Chrome
  and decode cleanly with <code>ffmpeg</code>/<code>ffprobe</code>.
</p>

---

## Features

- **Standard Opus in an Ogg container** — proper `OpusHead`/`OpusTags` packets and an EOS page; verified with `ffprobe`/`ffmpeg`.
- **Bundled libopus** — shipped via the `opusic-c` crate (libopus built from source, statically linked) with a hand-rolled RFC 3533 Ogg writer; no system `libopus-dev`/`libogg` package required.
- **Native capture** — `cpal` audio input: ALSA on Linux, WASAPI on Windows.
- **Two front ends** — a terminal CLI with `p`/`r`/`s`/`q` REPL controls and a native desktop GUI; no browser or web view involved.
- **Pause without gaps** — audio captured while paused is discarded at the boundary, not written as silence.
- **Readable microphone picker** — friendly device labels with ALSA alias deduplication, plus a "system default microphone" choice.
- **Local and offline** — capture, encoding, and file writing all happen on the machine; the GUI's fonts are embedded, so it needs no network at runtime.

## Platforms

| Platform | Release archive | Runtime notes |
|---|---|---|
| **Linux x86_64** (glibc 2.35+, e.g. Ubuntu 22.04+) | `kvr-v0.1.0-linux-x86_64.tar.gz` (`kvr`, `kvr-gui`) | ALSA capture; GTK 3 (native save dialog); OpenGL for the GUI renderer |
| **Windows x86_64** (MSVC) | `kvr-v0.1.0-windows-x86_64.zip` (`kvr.exe`, `kvr-gui.exe`) | WASAPI capture; MSVC runtime statically linked, no separate Visual C++ Redistributable required |
| **macOS** | not a release target | CoreAudio support exists in `cpal`; macOS builds are not verified by this project's CI |

## Quick start

Extract the platform archive and open a terminal in its root directory. The
commands below run the extracted binaries directly; no `PATH` setup is needed.

### Command-line interface

The CLI requires a subcommand; recording starts with `start`:

```bash
# Linux
./kvr start                                  # system mic, 96 kbps → ./recording-<timestamp>.opus
./kvr start -o my-take.opus --bitrate 128000   # explicit output path and bitrate
./kvr start --channels 1 --sample-rate 48000  # force channels / sample rate
./kvr start --dir ~/recordings
```

```powershell
# Windows
.\kvr.exe start
.\kvr.exe start -o .\my-take.opus --bitrate 128000
```

`start` runs in the foreground with an interactive REPL — type a key, then Enter:

| Key | Action |
|---|---|
| `p` | pause (audio captured while paused is not saved) |
| `r` | resume the same take |
| `s` | stop and finalize the `.opus` file |
| `q` | quit (stops and saves) |

`Ctrl+C` requests a stop and file finalization.

**External controls (Unix).** `start` writes a `session.json` (locate it
with `kvr where`), so a second shell can drive the running recorder
with signals:

```bash
./kvr status   # read session metadata (pid, output path, format)
./kvr pause    # SIGUSR1
./kvr resume   # SIGUSR2
./kvr stop     # SIGINT
```

`pause`/`resume`/`stop` are Unix-only (POSIX signals); on Windows, use the
foreground recorder's REPL controls instead. `status` and `where` are
available on both platforms. `--daemon` is a reserved
compatibility flag: it does not detach, creates no background service, and the
foreground REPL stays active.

**Verify a recording:**

```bash
ffprobe -v error -select_streams a:0 -show_entries stream=codec_name recording.opus
ffmpeg -i recording.opus -f wav out.wav
```

### Desktop GUI

```bash
# Linux
./kvr-gui
```

```powershell
# Windows
.\kvr-gui.exe
```

The native window provides:

- A microphone picker with readable device descriptions, plus a **System default microphone** choice
- An output path field with a native save-file dialog, and a bitrate slider: 16–256 kbps in 8 kbps steps (default 96 kbps)
- Record / Pause / Resume / Stop controls; a duration timer (pauses excluded), encoded-audio size, and a live RMS input meter with level history that freezes while paused
- Visible recording state, actionable error messages, and settings locked while a take is active; closing the window finalizes the current recording before exit

Capture and Opus encoding run on a separate thread so the UI can stay responsive.
The interface is native Rust (`egui`/`eframe` with the `glow` OpenGL renderer) — not a browser or embedded web UI.

**Visual design.** The workspace uses a pixel identity — navy/blue/cream/sand
palette, square frames, zero-blur offset shadows — with embedded Silkscreen,
VT323, and DotGothic16 fonts (SIL OFL) and Streamline Pixel artwork (CC BY
4.0); see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Recording behavior

- **Pause exclusion.** Pausing discards the partial frame at the boundary and everything captured while paused — it is never written as silence; resuming continues the same take with strictly increasing granule positions.
- **No overwrite.** The recorder never overwrites: if the chosen output path exists, a numeric suffix (`-1`, `-2`, `-3`, …) is appended automatically. The CLI default filename uses a one-second-resolution timestamp; the GUI default `<home>/recording.<ext>` is replaced with `<home>/<YYYYMMDD-HHMMSS>.<ext>` at the moment you press RECORD, so two presses inside the same second still produce two unique filenames (`<…>-1.<ext>`, `<…>-2.<ext>`). Explicit `-o` / `--output` paths and user-typed GUI stems get the same collision suffix. No `--force`/`--yes` flag is needed and an in-progress take is never silently lost to a new recording.
- **Local and private.** Capture, encoding, and writing all happen on your machine — no network, cloud, account, or telemetry. Besides recordings, the CLI stores session metadata in `session.json` (locate it with `kvr where`).

## Recording formats

`kvr start` defaults to Opus in an Ogg container (RFC 7845 audio in RFC 3533
pages), with PCM WAV and raw float32 little-endian available for tools that
can't read Ogg/Opus. The selection is a single flag.

### `--format` (default: `opus`)

| Value | Container / encoding | Default extension | Notes |
|---|---|---|---|
| `opus` (default) | Ogg container with `OpusHead` / `OpusTags` packets (RFC 7845 audio inside RFC 3533 pages) | `.opus` | the standard mode; verified by `ffprobe` |
| `wav`  | PCM WAV (RIFF `fmt ` chunk), interleaved | `.wav`  | decodes directly with every audio tool; no Opus encode |
| `raw`  | little-endian `f32` samples, one frame after another | `.f32le` | no header; pipes straight into analysis tools |

```bash
# Linux
./kvr start --format wav  -o meeting.wav        # 96 kbps Opus; full-rate PCM to disk
./kvr start --format raw  -o capture.f32le      # float32 little-endian, headerless
./kvr start --format opus -o take.opus          # explicit; same as the default
```

```powershell
# Windows
.\kvr.exe start --format wav -o .\meeting.wav
.\kvr.exe start --format raw -o .\capture.f32le
```

`--format` does not rewrite your `-o` path: if you pass `--format wav -o take.opus`
the recorder writes WAV data to `take.opus` (the filename you typed). The
extension is a hint for downstream tools, not a write-time gate. Pass
`--extension` to override the suffix on `-o` without renaming the file by hand.

### `--extension` (override)

```bash
./kvr start --format raw --extension pcmf32 -o meeting.analysis
```

The recorder builds its final output path by replacing the suffix on the
`-o`/`--output` argument with `--extension`. Empty `--extension` is rejected;
no extension on `-o` plus no `--extension` means the format's default
extension (`opus`, `wav`, `f32le`).

### Extension mapping (default)

| `--format` | Implied `--extension` |
|---|---|
| `opus` | `opus` |
| `wav`  | `wav`  |
| `raw`  | `f32le` |

`-o foo` with `--format opus` writes `foo.opus`; with `--format raw` it writes
`foo.f32le`. Add an explicit extension to `-o` to keep your own filename.

### Default stem and collision avoidance

- The CLI's auto-generated output path is `recording-<unix_seconds>.<ext>` in the
  current directory (or `--dir`), so a one-second-resolution timestamp plus the
  format's extension.
- The GUI's default output path is `~/recording.opus` on first launch (overridden
  by the user's chosen save-file name).
- Collision avoidance is automatic on both surfaces: if the resolved output path
  already exists, the recorder appends `-2`, `-3`, … before the extension and
  uses the first free name (`recording-1700000000.wav` →
  `recording-1700000000-2.wav`). The recorder never overwrites an existing file.

## Building from source

Stable Rust — [`rust-toolchain.toml`](rust-toolchain.toml) tracks the current stable release —
plus a C compiler and `cmake`, because the bundled libopus is built from source:

```bash
cargo build --locked --release
```

Binaries land in `target/release/` (`kvr`, `kvr-gui`; `*.exe` on
Windows).

- **Linux (Debian/Ubuntu):** `sudo apt install build-essential pkg-config libasound2-dev libgtk-3-dev cmake`
- **Linux (Fedora):** `sudo dnf install gcc gcc-c++ make pkgconf-pkg-config alsa-lib-devel gtk3-devel cmake`
- **Windows:** Visual Studio Build Tools with *Desktop development with C++*
  (MSVC + Windows SDK) and [CMake](https://cmake.org/) on `PATH`.

The repository's [Cargo configuration](.cargo/config.toml) enables `crt-static`
for Windows MSVC builds. Its target-specific CMake toolchain also enables
`OPUS_STATIC_RUNTIME` for bundled libopus; both are necessary to avoid
`VCRUNTIME140.dll` dependencies. Build from the repository root and do not
override these settings with `RUSTFLAGS` or a different CMake toolchain.
Existing Windows executables must be rebuilt to pick up this change.
For an older release reporting a missing runtime DLL, installing Microsoft's
[Visual C++ Redistributable](https://learn.microsoft.com/cpp/windows/latest-supported-vc-redist)
is a workaround; do not download individual DLLs from third-party sites.

`ffmpeg`/`ffprobe` are needed for the integration tests (see [CONTRIBUTING.md](CONTRIBUTING.md))
and for verifying recordings; the app itself does not need them.

## Releases and artifacts

Releases are produced by [`.github/workflows/release.yml`](.github/workflows/release.yml);
checks run on every push and PR via [`.github/workflows/ci.yml`](.github/workflows/ci.yml).
No release has been published yet — the table above shows the exact archive
names for the first release, `v0.1.0`.

Each archive has a single root directory
(`kvr-v0.1.0-linux-x86_64` / `kvr-v0.1.0-windows-x86_64`)
containing the two binaries plus `README.md`, `LICENSE`,
`THIRD_PARTY_NOTICES.md`, the three font `*-OFL.txt` notices, and
`assets/pixel-art-logo.png` so the packaged README logo resolves — not the
full repository source.

Checksums are published alongside: a `<archive>.sha256` sidecar for each archive plus a combined `SHA256SUMS`. Verify a download with:

```bash
# Linux
sha256sum -c kvr-v0.1.0-linux-x86_64.tar.gz.sha256
```

```powershell
# Windows (PowerShell)
Get-FileHash .\kvr-v0.1.0-windows-x86_64.zip -Algorithm SHA256
# compare against the value in the .sha256 sidecar / SHA256SUMS
```

Packaging is done by `scripts/package-release.sh` (Linux) and
`scripts/package-release.ps1` (Windows), which enforce the version match
against `Cargo.toml`, the archive layout, and checksum verification:

```bash
bash scripts/package-release.sh v0.1.0 --bin-dir target/release --output-dir dist
pwsh scripts/package-release.ps1 v0.1.0 -BinDir target/release -OutputDir dist
```

**Tagged release vs manual dispatch.** A GitHub release is created only when a
version tag matching the Cargo package version (currently `v0.1.0`) is
**pushed**, after both the Linux and Windows test/build/package jobs pass.
Manual dispatch of the release workflow — even selecting a tag in the
dispatch UI — builds and uploads the archives as workflow **artifacts only**;
it never creates a release.

<details>
<summary>First GitHub publication</summary>

The repository is not yet on GitHub. First publish:

1. Initialize and commit locally (`Cargo.lock` is tracked — intentionally part of the commit):

   ```bash
   git init -b main
   git add .
   git commit -m "kagantic-voice-recorder v0.1.0"
   ```

2. Create an empty repository on GitHub's web UI (no template README), set its URL as
   the origin (use the actual URL shown in the web UI), then push the branch and
   the matching version tag:

   ```bash
   git remote add origin <your-remote-url>
   git push -u origin main
   git tag v0.1.0
   git push origin v0.1.0
   ```

3. Watch the workflow: **both** the Linux and Windows test/build/package jobs
   must pass; the publish job then creates the `v0.1.0` release with the two
   archives and checksums. If either platform job fails, no release is created;
   fix the failure before publishing.
</details>

## Codec and device details

<details>
<summary>Container format</summary>

Each output file is one logical Ogg stream (serial = 1) carrying, in
order: the `OpusHead` identification packet (channel count, pre-skip,
input sample rate), the `OpusTags` comment packet, Opus audio packets —
20 ms frames at 48 kHz, granule position = frames × 960 samples — and a
final EOS page with the stream-end flag. Page headers and the CRC-32
parameters follow RFC 3533; the implementation is in
[`src/ogg.rs`](src/ogg.rs).

</details>

<details>
<summary>Microphone selection</summary>

- `kvr` lists `cpal` capture devices with friendly labels; recording passes the unchanged backend ID to capture, so a label is never treated as a device identifier.
- On Linux, aliases of the same ALSA capture endpoint are collapsed, direct routes are preferred over channel-conversion aliases, and generic `default`/`pulse`/`pipewire` routes are not listed twice.
- **System default microphone** follows system routing, including a configured PulseAudio/PipeWire default.
- Hardware that cannot currently be opened for capture is not listed — refresh after reconnecting it or releasing it from another app; a capture-capable card can expose a jack even when nothing is plugged in.
- Device disconnection after listing produces an explicit capture-selection error.

Implementation: [`src/devices.rs`](src/devices.rs).
</details>

<details>
<summary>Windows PE verification status</summary>

The Windows release archive is built on the `windows-2022` GitHub-hosted
runner with the MSVC + Windows SDK toolchain (Visual Studio Build Tools,
"Desktop development with C++") plus CMake. `scripts/package-release.ps1`
refuses to overwrite an existing archive, requires every binary to be a
real PE x86-64 file, rejects Visual C++ runtime DLL imports in either executable,
validates the single-root directory structure of the ZIP, and writes a SHA-256
sidecar. Dependency inspection uses `dumpbin` from Visual Studio Build Tools
on Windows, or `objdump` (binutils) when cross-packaging on Linux.
The Windows CLI is smoke-tested
(`--help` / `--version`) only on the `windows-2022` runner, where it must
report exactly `kvr <version>` before the archive is uploaded. Linux
runners cannot execute Windows PE binaries, so the per-platform CLI smoke
remains platform-local.

To verify the archive on a Windows machine:

```powershell
Expand-Archive .\kvr-v0.1.0-windows-x86_64.zip
.\kvr-v0.1.0-windows-x86_64\kvr.exe --version
Get-FileHash .\kvr-v0.1.0-windows-x86_64.zip -Algorithm SHA256
```

The expected `kvr --version` output is `kvr 0.1.0` and the hash
must match the value in the sidecar / `SHA256SUMS`.

</details>

## License and documentation

- Source code: **AGPL-3.0-or-later** — [`LICENSE`](LICENSE)
- [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) — bundled fonts (SIL OFL), Streamline icons (CC BY 4.0), the user-authored [`assets/pixel-art-logo.png`](assets/pixel-art-logo.png) (excluded from the source-code licenses — the project grants no additional redistribution rights to it), and the Opus codec components
- [CONTRIBUTING.md](CONTRIBUTING.md) — prerequisites, quality gates, and testing notes
- [`.github/workflows/ci.yml`](.github/workflows/ci.yml) — checks on every push and pull request; [`.github/workflows/release.yml`](.github/workflows/release.yml) — tag releases and manual artifact builds
