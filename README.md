# Gwar

**Gwar** is an open-source, self-hosted voice chat. Run your own server,
talk from the browser or the desktop app, and keep your friends who still use
TeamSpeak in the same conversation.

*Gwar* is Polish for the buzz of many voices talking at once.

![Gwar desktop app](docs/screenshot.png)

## Why

TeamSpeak works, but it moves slowly and its newer clients are hard to use.
Discord is polished, but it is a closed service you cannot host yourself.
Gwar aims for the middle: a server you own, a modern and simple interface, and
no lock-in, including compatibility with the TeamSpeak ecosystem people already
use.

## What it does today

- **Voice with very low latency.** Opus is forwarded by a WebRTC SFU without
  being decoded or re-encoded; the server only decides who hears whom.
- **Always on, like Discord.** You stay connected to a server without being in
  voice: read and write any channel's chat, see who is in which voice channel,
  join voice when you want to talk. Unread counts follow you across devices.
- **Channels and chat.** Nested channels, passwords, user limits, persistent
  channel history, private and server-wide messages, a member list with online
  and offline users.
- **Your identity is a key, not an account.** Every client holds an Ed25519
  key; there is no central sign-up and it works on any Gwar server. Admin
  rights are handed out with one-time tokens.
- **Web and desktop clients with one UI.** A React web app, and a Tauri 2
  desktop app with a native audio engine (Opus with FEC, jitter buffer with
  loss concealment), global push-to-talk and a tray icon. English and Polish.
- **TeamSpeak in both directions.**
  - Official TeamSpeak 3 and 6 clients can join a Gwar server (optional, see
    below). They see the same channels and people, and talk and chat with
    everyone else.
  - The desktop app can connect to any existing TS3/TS6 server with the same
    interface.

Gwar is early software: it works and is tested end to end, but expect rough
edges and breaking changes.

## Quick start

Requirements: Rust (stable), Node 22+, pnpm, and libopus for the native clients
(`brew install opus` or `apt install libopus-dev libasound2-dev`).

```bash
pnpm install
pnpm --filter web build
cargo run -p vc-server -- --data-dir data --web-root apps/web/dist
```

Open <http://localhost:8790>. On first start the server prints a one-time
**admin token**; redeem it from the server menu ("Use token"). Get another one
with `cargo run -p vc-server -- --data-dir data admin-token`.

Desktop app: `pnpm --filter desktop dev`.

## Self-hosting

Open **TCP 8790** (or 443 behind a reverse proxy with HTTPS, which browsers
require for microphone access) and **UDP 9987** for WebRTC voice.
`--public-ip` sets the address announced to clients when the server is behind
NAT. `vc-server --help` lists every option.

## TeamSpeak compatibility

Official TeamSpeak clients only accept servers that hold a license issued by
TeamSpeak, so Gwar does not imitate a TeamSpeak server. Instead, with
`--teamspeak --accept-teamspeak-license` it downloads the official TeamSpeak 3
server from TeamSpeak (checksum-verified), runs it as a hidden child process
and bridges it:

- channels, the server name and the welcome message are managed by Gwar and
  mirrored to TeamSpeak;
- TeamSpeak users appear in Gwar like everyone else, with a TS3/TS6 badge;
- a Gwar user who shares a channel with TeamSpeak users (or chats with one
  privately) gets a stand-in client on TeamSpeak, so TeamSpeak users see, hear
  and message them; everyone else is listed in the channel description. Voice
  crosses the bridge as Opus with about a millisecond of overhead.

TeamSpeak clients connect to `your-host:9987` (UDP); Gwar's WebRTC voice then
moves to 9988, so open both ports. On ARM64 hosts the x86-64 TeamSpeak server
runs under emulation (`apt install qemu-user libc6-amd64-cross
libstdc++6-amd64-cross`, or box64). Only one free-licensed TeamSpeak server may
run per machine.

**License note.** The TeamSpeak server is third-party software under its own
license, which you accept when enabling this option; Gwar does not ship it.
Its free license allows 32 slots (TeamSpeak users plus Gwar stand-ins) and is
for **non-commercial use only**; commercial operators need a license from
TeamSpeak. Gwar itself has no slot limits. Gwar is not affiliated with
TeamSpeak. TeamSpeak support is a cargo feature (`teamspeak`, on by default);
build without it with `--no-default-features`.

## Project layout

| Path | What it is |
| --- | --- |
| `crates/vc-server` | Server: core (state, permissions, SQLite), WebSocket gateway, WebRTC SFU, TeamSpeak bridge |
| `crates/vc-proto` | The `vc/1` protocol (Rust types, generated TypeScript types) |
| `crates/vc-client` | Native client: protocol, voice engine, TeamSpeak mode |
| `apps/web` | The shared UI (React), in the browser and the desktop app |
| `apps/desktop` | Tauri 2 desktop app: native voice, global push-to-talk, tray |
| `scripts/deploy-vm.sh` | Deploys a server to a Linux host over SSH |

More in [docs/architecture.md](docs/architecture.md).

## Tests

```bash
cargo test --workspace          # includes end-to-end WebRTC voice tests
pnpm --filter web test          # UI unit tests
pnpm --filter web e2e           # Playwright: two browsers against a real server
VC_TS3_DIR=/tmp/ts3 cargo test -p vc-client --test teamspeak_mode --test teamspeak_interop -- --test-threads=1
                                # against the real TeamSpeak server (downloaded into VC_TS3_DIR)
```

## Roadmap

- **Mobile app** (iOS and Android).
- **Streaming**: screen sharing and video, peer to peer and through the server.
- **Better audio on desktop**: noise suppression and echo cancellation.
- **Fully native clients** later, keeping the same protocol.
- **Roles and permissions UI**, notifications and mentions.
- **More TeamSpeak options**: bring your own TeamSpeak license for more slots,
  and a mixed stand-in so Gwar users can still talk when the free slots run out.

Ideas and pull requests are welcome; open an issue to discuss bigger changes.

## License

MIT, see [LICENSE](LICENSE). Third-party components keep their own licenses,
see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
