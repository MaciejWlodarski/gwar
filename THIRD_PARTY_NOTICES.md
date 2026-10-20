# Third-party components

This project's own code is licensed under MIT. Third-party components retain
their respective licenses and attribution.

The desktop app carries this file, the project's `LICENSE` and
`THIRD_PARTY_LICENSES.md` in its bundle: the licenses of every Rust crate it
ships (native, and in its MLS WebAssembly module), of the Opus library built
into it, and of the JavaScript packages and the font of its interface, written
at build time by `scripts/licenses/build.mjs` (cargo-about, and Vite's list of
the bundled packages). The web app serves its JavaScript packages' and font's
licenses at `/licenses.md`. The components under the Mozilla Public License 2.0
are used unmodified; their source code is at the repositories listed with them.

## ReSpeak / tsclientlib

The TeamSpeak client mode and the TeamSpeak bridge depend on
[ReSpeak/tsclientlib](https://github.com/ReSpeak/tsclientlib) (MIT OR
Apache-2.0), pinned to commit `ee3bc6f45a7137db7793ba5593a321df400d53e5` in the
Cargo manifests and lockfiles.

## Noise suppression model

The noise suppression uses **DeepFilterNet3** (low-latency model
`DeepFilterNet3_ll_onnx.tar.gz`) and its runtime `libDF` from
[Rikorose/DeepFilterNet](https://github.com/Rikorose/DeepFilterNet) (MIT OR
Apache-2.0, code and published models), through our fork
[MaciejWlodarski/DeepFilterNet](https://github.com/MaciejWlodarski/DeepFilterNet)
(branch `gwar/tract-0.23`: libDF ported to tract 0.23), pinned to commit
`24d1b6df281f0e8208aa2eeb5d2bb942fb0737d0` (upstream `d375b2d8309e0935d165700c91da9de862a99c31`
plus the port), run with [sonos/tract](https://github.com/sonos/tract) (MIT OR
Apache-2.0). The desktop app embeds the model; the web app serves it, with the
runtime built as WebAssembly, and downloads it when suppression is first on.

## Echo cancellation and gain control

The desktop app's echo cancellation and automatic gain control are WebRTC's audio
processing (AEC3, AGC2), through
[dignifiedquire/sonora](https://github.com/dignifiedquire/sonora) 0.2.0, its port
to Rust, under the BSD-3-Clause license below. Browsers run their own copy of the
same processing for the web app.

```text
Copyright (c) 2011, The WebRTC Project Authors. All rights reserved.
Copyright (c) 2016, Arun Raghavan and contributors. All rights reserved.
Copyright (c) 2026, dignifiedquire. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

  * Redistributions of source code must retain the above copyright
    notice, this list of conditions and the following disclaimer.

  * Redistributions in binary form must reproduce the above copyright
    notice, this list of conditions and the following disclaimer in
    the documentation and/or other materials provided with the
    distribution.

  * Neither the name of Google nor the names of its contributors may
    be used to endorse or promote products derived from this software
    without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## TeamSpeak server

With `--teamspeak`, the server downloads the official TeamSpeak 3 server from
TeamSpeak's download site on the operator's machine and runs it under the
TeamSpeak server license, which the operator accepts with
`--accept-teamspeak-license`. It is not redistributed with this project.

Other Rust and JavaScript dependencies are installed separately from the package
registries or pinned upstream repository; their package metadata carries their
own licenses. Prebuilt third-party applications and runtime credentials are
not part of this source repository.

TeamSpeak is a trademark of TeamSpeak Systems GmbH. Gwar is an independent
project, not affiliated with or endorsed by TeamSpeak; the name is used only to
describe compatibility.
