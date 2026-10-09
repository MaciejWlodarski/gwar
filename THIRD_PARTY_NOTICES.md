# Third-party components

This project's own code is licensed under MIT. Third-party components retain
their respective licenses and attribution.

## ReSpeak / tsclientlib

The TeamSpeak client mode and the TeamSpeak bridge depend on
[ReSpeak/tsclientlib](https://github.com/ReSpeak/tsclientlib) (MIT OR
Apache-2.0), pinned to commit `ee3bc6f45a7137db7793ba5593a321df400d53e5` in the
Cargo manifests and lockfiles.

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
