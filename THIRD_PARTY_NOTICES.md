# Third-Party Notices

The project code in this repository (the `kvr` CLI, `kvr-gui`, and the
`kvr_recorder` library) is Copyright (c) 2026 Kagan Erkan and is licensed
under the GNU Affero General Public License, version 3 or later
(`LICENSE`). The third-party components below are used in and distributed
with the project; each keeps its own license.

## Fonts (SIL Open Font License 1.1)

The following fonts are embedded in the GUI and shipped under
`assets/fonts/`. The full OFL 1.1 text is included in the accompanying
`*-OFL.txt` file for each font.

- **Silkscreen** — `assets/fonts/Silkscreen-Regular.ttf`
  - Copyright 2001 The Silkscreen Project Authors (https://github.com/googlefonts/silkscreen)
  - License: OFL 1.1 — `assets/fonts/Silkscreen-OFL.txt`
- **VT323** — `assets/fonts/VT323-Regular.ttf`
  - Copyright 2011, The VT323 Project Authors (peter.hull@oikoi.com)
  - License: OFL 1.1 — `assets/fonts/VT323-OFL.txt`
- **DotGothic16** — `assets/fonts/DotGothic16-Regular.ttf`
  - Copyright 2020 The DotGothic16 Project Authors (https://github.com/fontworks-fonts/DotGothic16)
  - License: OFL 1.1 — `assets/fonts/DotGothic16-OFL.txt`

## Icons (Creative Commons Attribution 4.0 International)

The microphone and folder glyphs in `assets/icons/` are artwork from
Streamline's **Pixel – Free** collection (https://www.streamlinehq.com/icons/pixel),
identified by Streamline as available under CC BY 4.0
(https://creativecommons.org/licenses/by/4.0/):

- **Music Microphone 2** — https://www.streamlinehq.com/icons/download/music-microphone-2--15576
  — shipped as `assets/icons/microphone.png`
- **Content Files Folder Open** — https://www.streamlinehq.com/icons/download/content-files-folder-open--15564
  — shipped as `assets/icons/folder.png`

Both were exported from Streamline as 32×32 white PNGs and are displayed at
their native size in the GUI without pixelation filters or geometry changes.

## Logo

`assets/pixel-art-logo.png` is an original brand asset authored by Kagan Erkan.
The logo is excluded from the source-code licenses. This project does not
grant additional redistribution rights to this personal-brand asset.

## Opus codec and Rust bindings (bundled in the binaries)

The recorder statically links the Opus encoder through the `opusic-c` /
`opusic-sys` crates (`Cargo.toml`: `opus = { package = "opusic-c", version =
"1.6" }`). `opusic-sys` bundles the libopus C source, which is compiled and
linked into both binaries. The license texts below are reproduced verbatim
from the license files of the installed crate versions
(opusic-c 1.6.1, opusic-sys 0.7.5).

### libopus (bundled C source; target version 1.6.1)

Vendored with `opusic-sys` 0.7.5 (https://github.com/DoumanAsh/opusic-sys;
README: "Target version 1.6.1", https://github.com/xiph/opus/releases/tag/v1.6.1).
License text from the bundled `opus/COPYING` file (BSD-style; identical to the
`opusic-sys` crate's `LICENSE` file, which states the crate has the same
license requirements as the C source code):

```text
Copyright 2001-2023 Xiph.Org, Skype Limited, Octasic,
                    Jean-Marc Valin, Timothy B. Terriberry,
                    CSIRO, Gregory Maxwell, Mark Borgerding,
                    Erik de Castro Lopo, Mozilla, Amazon

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of Internet Society, IETF or IETF Trust, nor the
names of specific contributors, may be used to endorse or promote
products derived from this software without specific prior written
permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Opus is subject to the royalty-free patent licenses which are
specified at:

Xiph.Org Foundation:
https://datatracker.ietf.org/ipr/1524/

Microsoft Corporation:
https://datatracker.ietf.org/ipr/1914/

Broadcom Corporation:
https://datatracker.ietf.org/ipr/1526/
```

### opusic-c 1.6.1 (high-level Rust bindings)

https://github.com/DoumanAsh/opusic-c — license text from the crate's
`LICENSE` file (BSD 3-Clause):

```text
BSD 3-Clause License

Copyright (c) 2024, Douman

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived from
   this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### opusic-sys 0.7.5 (sys crate)

https://github.com/DoumanAsh/opusic-sys — "This crate has the same license
requirements as C source code" (README); its `LICENSE` file contains the
identical libopus text reproduced above. All modifications to the bundled
source are described in the crate's `opus.patch`.

## Linux AppImage shared libraries

The AppImage also carries dynamically linked GTK and its native dependencies.
These libraries retain their distribution-provided licenses; the project's AGPL
license does not replace them.

The packaging process includes copyright/license files and a `provenance.json`
inventory under `usr/share/doc/kagantic-voice-recorder/libraries/` inside the
image. Ubuntu/Debian packages are identified by GNU build IDs, with exact binary
and source versions and source-archive links. Freedesktop SDK builds include the
SDK's original license files and source manifest with source URLs/revisions.
Packaging adjusts ELF RPATH and may strip debug symbols; it does not change
library source code.

Run the AppImage with `--appimage-extract` to inspect these files. The extracted
`usr/lib/` shared libraries can be replaced with interface-compatible modified
versions; run the extracted `AppRun` to use that copy.
