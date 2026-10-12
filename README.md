# Gwar

**Gwar** is an open-source, self-hosted voice chat. Run your own server,
talk from the browser or the desktop app, and keep your friends who still use
TeamSpeak in the same conversation. It is built to stay trustworthy even when
the server you join is not: your identity is a key only you hold, and every
message is signed (see [Security](#security)).

**Use it:** the web app at <https://gwar.maciejwlodarski.com> connects to any
Gwar server (the project's own is `voice.maciejwlodarski.com`); so does the
desktop app.

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
  key; it works on any Gwar server without signing up anywhere.
- **Optional Gwar Connect account** to be the same person on the web, the
  desktop and (later) the phone. Connect never sees your password or your
  key: it stores your identity encrypted, each device gets its own key
  certified by your identity, and a lost device is revoked without changing who
  you are. Servers verify devices themselves and keep working if Connect is
  down. See [docs/connect.md](docs/connect.md).
- **Moderation.** Roles with any permissions and colors, bans (timed or
  permanent, by identity or IP), invite links that can grant a role, server
  settings, removing members and cleaning up the inactive ones. Nobody can hand
  out more than they hold.
- **Signed chat.** Every message, edit, deletion and attachment is signed by
  its author and verified by every client, so a server cannot put words in
  anyone's mouth. See [Security](#security).
- **Chat like you expect.** Mentions with notifications, editing and deleting,
  images, video, audio and files (drag and drop or paste).
- **One web app, plus a desktop app, with one UI.** The project hosts the web
  app; it connects to any server. The Tauri 2 desktop app has a native audio
  engine (Opus with FEC, jitter buffer with loss concealment), global
  push-to-talk and a tray icon. English and Polish.
- **TeamSpeak in both directions.**
  - Official TeamSpeak 3 and 6 clients can join a Gwar server (optional, see
    below). They see the same channels and people, and talk and chat with
    everyone else.
  - The desktop app can connect to any existing TS3/TS6 server with the same
    interface.

Gwar is early software: it works and is tested end to end, but expect rough
edges and breaking changes.

## Security

Gwar starts from the assumption that everything is public: the code, the
protocol, and possibly a modified server you join by mistake. Nothing depends
on hidden behavior; security comes only from keys that never leave their owner.
A server is trusted to route voice and deliver messages, but never with who you
are or what you said: even a hostile server cannot log in as you elsewhere,
forge your messages or show a fake verified name without a visible warning.

### Your identity is a key

- Every identity is an **Ed25519 key pair generated on your device**. Your uid
  is a hash of the public key and your `#tag` comes from the uid, so nobody, not
  even a server operator, can choose, take or fake them.
- Joining a server needs no account, e-mail or sign-up anywhere.

### Logging in: mutual proof, no relaying

- Every server has its own **Ed25519 server key**. Invite links carry its full
  fingerprint, and clients pin it for each server address. If the key changes,
  login stops until you check it with the owner; there is no silent rotation
  and no fallback to a weaker login.
- Login is a **mutual challenge-response**: your client signs the exact address
  it dialed, the server key it expects and fresh random values from both sides,
  and the server must answer with a proof signed by its own key. A malicious
  server cannot replay your login to another server, and cannot pretend to be a
  server whose key it does not hold.
- Server passwords and invites are sent only after the server has proven itself.

### Encrypted connections

- Everything travels over **TLS** (`https://`, `wss://`); the server can obtain
  and renew Let's Encrypt certificates by itself.
- Voice uses **WebRTC, encrypted with DTLS-SRTP**.
- Plaintext is allowed only to loopback for development, or through an explicit
  per-server LAN option in the desktop app, which shows a permanent red warning
  while connected.

### Every message is signed

- Each message, edit, deletion and attachment is **signed by its author's key**
  and bound to the server's key, the conversation and the message id. A server
  cannot alter a message, move it to another channel or present it as someone
  else's without clients noticing.
- **Edits form a hash chain**, and clients keep local checkpoints that refuse
  rollbacks and forks of messages they have already seen.
- Attachments are **checked against their signed hash** before they are shown
  or saved.
- Until its signature is checked a message is visibly pending; anything that
  fails or cannot be fully verified carries a warning. Notifications and
  mentions fire only for verified messages.

### Gwar Connect: an account that never sees your secrets

- **Your password never leaves your device.** It is stretched there with
  **Argon2id** (64 MiB, 3 passes) into two keys: one encrypts your account key
  with **AES-256-GCM**, the other signs you in, and Connect keeps only an
  Argon2 hash of it. A leaked Connect database lets nobody sign in; reading an
  account key would take an offline guessing attack on that account's password.
- A **recovery code** with 160 random bits, shown once, restores access if you
  forget the password.
- Every device has **its own key, certified by your account key** for at most
  90 days. A lost device is revoked with a signed revocation; servers then
  disconnect it, and you stay the same person.
- Your **vault** (profile, TeamSpeak identities) is encrypted on your devices
  with AES-256-GCM under a key derived from your account key; Connect stores
  only ciphertext.
- An `@handle` badge is a **signed attestation** that your client checks
  against the key of its own Connect service, so a server cannot invent one.
  Servers verify devices themselves and keep working when Connect is down.

### Hardened against hostile input

- Clients validate every server message, with size and count bounds, before
  using it. Servers limit frame sizes before parsing and rate-limit
  connections that have not logged in yet.
- The web app and the desktop interface run under a **Content-Security-Policy**.
  Uploaded files are served with a sandboxing CSP and `nosniff`, and only from
  the server's own origin.
- Images are decoded only near the visible part of the chat and within a memory
  budget; an image that decodes larger than its header claims is dropped.
- Storage is capped per member and in total, so free identities cannot fill the
  disk.

### What Gwar does not protect, yet

Being honest about the limits is part of the design:

- **The server you are on can read its content**: messages, direct messages,
  files and voice. Voice goes through the server (an SFU) and is not
  end-to-end encrypted. For private conversations, use a server you trust, or
  run your own.
- **Direct messages are not end-to-end encrypted yet**; per-device end-to-end
  encryption is on the roadmap.
- Voice is not signed, and presence, nicknames, channel names and roles are the
  server's word.
- The web app is only as trustworthy as its hosting, which serves the code on
  every load. Desktop builds are not signed yet.
- TeamSpeak users have no Gwar keys, so their messages are shown as unsigned.

The full threat model, with every mechanism and its limits, is in
[docs/security.md](docs/security.md).

## Names: nickname, #tag and @handle

A member is shown as **nickname** `@handle #tag`, for example
**Dominika** `@dominika #kd5wx7rmq2`.

- **Nickname**: what people see in chat and in the member list. It is chosen
  per server and the same on all your devices. Two people may have the same
  nickname. Your identity has a global nickname (a random `gwar-#####` at
  first, change it in Settings → Identity). A server uses it only the first
  time you join. After that, change it on that server with *Change nickname*.
  Roles with the *Change member nicknames* permission can rename others.
  History always shows the current nickname.
- **#tag**: derived from your identity key (lowercase base32 of the uid). The
  server uses the shortest prefix that is unique on that server, at least
  10 characters. Nobody can choose it, change it or take someone else's,
  and every identity has one, with or without an account. Use it to tell apart
  people with the same nickname. The mention picker shows it too.
- **@handle**: your [Gwar Connect](docs/connect.md) account name, shown with a
  verified badge. Your client checks Connect's signed handle attestation against
  the key fetched from its own configured Connect service and the member's uid.
  The next lookup is allowed
  after 24 hours on success, one hour on 404 (which clears the handle), or five
  minutes on error; the five-minute guard is saved before HTTP, across devices
  and restarts. Errors keep the cached attestation while Connect is down; expired
  ones are hidden.
  It is only as trustworthy as Connect, while the #tag needs no one.

## Download

Prebuilt apps are on the [GitHub Releases page](https://github.com/MaciejWlodarski/gwar/releases):

- **Desktop app:** macOS (universal `.dmg`), Windows (`.exe` or `.msi`) and
  Linux (`.AppImage` or `.deb`). Builds may be unsigned: macOS and Windows then
  warn at the first start, and [docs/releasing.md](docs/releasing.md) shows how
  to open them anyway.
- **Server:** `vc-server` and `gwar-connect` for Linux x86-64 and ARM64
  (`gwar-server-*.tar.gz`), so you do not need Rust to run your own. Unpack and
  continue with "Run a server" below, using `./vc-server` instead of
  `./target/release/vc-server`.

`SHA256SUMS` on each release lists the checksum of every file. Maintainers: see
[docs/releasing.md](docs/releasing.md) for how a release is made.

## Run a server

You run only the server; people connect with the official web app or the
desktop app. The web app is served over HTTPS, so browsers can only reach
servers that have HTTPS too, which the server can do on its own:

```bash
cargo build --release -p vc-server
./target/release/vc-server --data-dir data --domain voice.example.com --acme-email you@example.com
```

With `--domain` the server gets a certificate from Let's Encrypt and renews it
by itself (it answers the challenge on port 443, so nothing else is needed),
listens for HTTPS on 443 and redirects plain HTTP from port 80. Open
**TCP 443** (and 80 for the redirect) and **UDP 9987** for voice. Already have a
certificate or a reverse proxy? Use `--tls-cert`/`--tls-key`, or plain HTTP on
`--http` behind your proxy. `--public-ip` sets the address announced for voice
when the server is behind NAT; `vc-server --help` lists every option.

On first start the server prints a one-time **admin token**; open the web app,
connect to `voice.example.com` and redeem it from the server menu ("Use
token"). Get another one with `vc-server --data-dir data admin-token`.

TLS is required by default. Plain `ws://` is allowed for loopback development.
In the desktop app, a per-server **Insecure connection (LAN)** toggle also
permits plaintext to other hosts. While connected a persistent red warning
explains that anyone on the network can impersonate the server or take over
the session. Pins and message signatures do not secure a plaintext session.

The server generates `server-key.ed25519` in its data directory once (a
32-byte Ed25519 seed, mode 0600 on Unix). **Back it up with the database**;
losing it changes the server's identity. Never share the seed. Print the public
key with `vc-server --data-dir data server-key`. Invite links include this full
public-key fingerprint. Clients store a pin for each canonical WebSocket
endpoint after verifying the first server proof. A changed key blocks login:
the user must check it with the owner and explicitly choose the dangerous
option to forget the old pin and trust the new key. No silent rotation or
legacy login fallback. The server menu's **Server key** shows the fingerprint.

Login signatures bind the full endpoint, including scheme, host, effective
port and WebSocket path. Configure the exclusive accepted set using:

- `--domain`: `wss://<domain>/ws` for each configured domain;
- `--public-url`: maps HTTP(S) to WS(S), retains host/port and uses `/ws`;
- `--public-endpoint`: repeatable full endpoint URLs, including alias paths
  (or comma-delimited `VC_PUBLIC_ENDPOINTS`);
- explicit `--public-ip`: a global unicast IP adds `wss://<ip>[:port]/ws` only
  when vc-server terminates TLS itself. It uses the actual listening port,
  omitted only for 443. Behind a reverse proxy it adds no login endpoint;
  configure `--public-url` or `--public-endpoint`.
  Browser IP access needs a certificate valid for that IP.

Behind a reverse proxy, set its public URL, for example:

```bash
vc-server --http 127.0.0.1:8800 --public-url https://voice.example.com --client-ip-header x-real-ip
```

`--client-ip-header` names the header your proxy sets to the client's address
(nginx: `proxy_set_header X-Real-IP $remote_addr;`). It is believed only from
loopback; name only a header the proxy always overwrites.

Hostnames are lowercase without a trailing dot, IPv6 is bracketed, and default
WS/WSS ports are omitted. No credentials, query or fragment. Request headers
never add endpoints. Private, loopback and link-local `--public-ip` values
never auto-authorize login. Configure loopback explicitly for development:

```bash
vc-server --public-endpoint ws://127.0.0.1:8790/ws
```

An empty set refuses startup. `--insecure-any-host` (`VC_INSECURE_ANY_HOST`)
accepts any canonical endpoint and logs a loud warning. It is only for isolated
local development/tests, never staging or ordinary LAN use with valuable
identities. It disables relay protection even when pins exist.

Servers accept Gwar Connect devices out of the box and fetch the list of
revoked devices from the official Connect service every five minutes
(`--connect-url` points elsewhere, `--no-connect` turns it off). They also confirm
Connect handles asynchronously (rechecked daily, sooner after a failure);
`--no-connect` clears that cache. Back up the database while running with `vc-server --data-dir data backup copy.sqlite3`.

Admins (or anyone with the "Remove members" permission) can remove a member
from the member list, or clean up people not seen for a while in Server
settings. Removing is not a ban: the member's record, roles and read marks go
(their messages and files too, if asked) and they can join again as a new
member. The same works from the command line, straight on the database:

```bash
vc-server --data-dir data members list [--inactive-days 90]
vc-server --data-dir data members prune --inactive-days 90 --dry-run
vc-server --data-dir data members prune --inactive-days 90 [--include-grouped] [--delete-messages]
vc-server --data-dir data members remove [--delete-messages] [--dry-run] -- <uid>...
vc-server --data-dir data members merge [--dry-run] -- <from-uid> <into-uid>
```

`list` and removal/prune `--dry-run` are safe while the server runs. A real
`prune` must run with the server stopped (the server holds `data/vc.lock`; the command refuses
to run while it is held). Members with a role besides Member are kept unless
`--include-grouped` is given. `list` includes the member tag and confirmed
Connect handle. `merge` moves the source's unsigned messages, uploads, roles,
mentions in unsigned messages, read marks and UID bans to an existing target,
preserving the target's nickname. Signed messages keep their original `author_uid`
and mentions because the signature binds them; the CLI reports only unsigned
messages and mentions moved.
It is transactional and requires the server stopped, including `--dry-run`.
Put UIDs beginning with `-` after `--`, as in the examples above.

## Develop

Requirements: Rust (stable), Node 22+, pnpm, and libopus for the native clients
(`brew install opus` or `apt install libopus-dev libasound2-dev`).

```bash
pnpm install
cargo run -p vc-server -- --data-dir data --public-endpoint ws://localhost:8790/ws --public-endpoint ws://127.0.0.1:8790/ws --public-endpoint ws://localhost:5173/ws --public-endpoint ws://127.0.0.1:5173/ws
pnpm --filter web dev                            # web app on http://localhost:5173
pnpm --filter desktop dev                        # desktop app
```

`--web-root apps/web/dist` makes the server serve a built web app itself (handy
for development or a private network). Uploads from a web app on another
origin are allowed for the official web app and for `--web-origin` entries.

Microphone permission on the desktop app: macOS asks on first use (the bundled
app carries `NSMicrophoneUsageDescription` and the audio-input entitlement), and
Windows or macOS users who blocked it get an "Open privacy settings" button.
`tauri dev` runs an unbundled binary, so macOS attributes the prompt (and the
System Settings entry) to the terminal you started it from; test the real flow
with `pnpm --filter desktop build`. To reset a decision while testing:
`tccutil reset Microphone app.gwar.desktop`.

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
moves to 9988, so open both ports, and TCP 30033 for TeamSpeak file transfer
(icons, avatars, channel files; `--teamspeak-filetransfer` moves it). On ARM64 hosts the x86-64 TeamSpeak server
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
| `crates/gwar-connect` | Gwar Connect, the optional account service |
| `apps/web` | The shared UI (React): the hosted web app and the desktop app's UI |
| `apps/desktop` | Tauri 2 desktop app: native voice, global push-to-talk, tray |
| `scripts/deploy-vm.sh` | Deploys the server and Connect to a Linux host over SSH |

More in [docs/architecture.md](docs/architecture.md), [docs/connect.md](docs/connect.md),
[docs/security.md](docs/security.md) and [docs/deploy.md](docs/deploy.md).

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
- **Direct messages between Connect accounts**, end-to-end encrypted per
  device and relayed when the other side is offline.
- **More TeamSpeak options**: bring your own TeamSpeak license for more slots,
  and a mixed stand-in so Gwar users can still talk when the free slots run out.

Ideas and pull requests are welcome; open an issue to discuss bigger changes.

## License

MIT, see [LICENSE](LICENSE). Third-party components keep their own licenses,
see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
