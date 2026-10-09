# Architecture

```mermaid
flowchart LR
  Web[Browser<br/>apps/web] -- WSS vc/1 --> GW
  Desk[Desktop app, Tauri<br/>apps/web + vc-client] -- WSS vc/1 --> GW
  TS3[Official TS3/TS6 clients] -- UDP 9987 --> TSS
  Web -- WebRTC Opus --> SFU
  Desk -- WebRTC Opus --> SFU
  subgraph vc-server [vc-server]
    GW[WebSocket gateway] --> CORE[core actor]
    SFU[WebRTC SFU, str0m]
    CORE -- routing snapshot --> SFU
    CORE <-- events / remote sessions --> BR[TeamSpeak bridge]
    SFU -- our users' voice --> BR
    BR -- TeamSpeak voice --> SFU
    CORE --> DB[(SQLite)]
  end
  BR -- ServerQuery + puppets --> TSS[official TS3 server<br/>child process]
  Desk -. TeamSpeak mode: TS UDP .-> EXT[Any TS3/TS6 server]
```

## Principles

- **One owner of state.** `core` is an actor (a single tokio task): every
  change to channels, users and permissions goes through it in order, so every
  client sees the same sequence of events, without locks. Transports
  (WebSocket, TeamSpeak) are thin: they translate to `vc/1` and back.
- **Audio is never decoded on the server.** Opus frames are forwarded as they
  are (SFU). The server only decides *who hears whom*, from a routing snapshot
  the core publishes (`ArcSwap`, lock-free reads). Clients decode and mix
  themselves (TS3 does it natively; browsers and the desktop app have 8 fixed
  receive "slots" assigned to active speakers).
- **Speech gate on the server.** Browsers send their audio level (RFC 6464);
  the server drops silence, drives the "talking" indicator and ends talk spurts
  for TeamSpeak clients.

## The `vc/1` protocol

JSON over a WebSocket (`/ws`). The full, commented definition is
[`crates/vc-proto/src/lib.rs`](../crates/vc-proto/src/lib.rs); the TypeScript
types are generated with `cargo test -p vc-proto --features ts`.

1. Server: `challenge {nonce}`. Client: `hello` with its Ed25519 public key and
   a signature of `"vc/1 hello\n" + nonce + "\n" + public_key`. Reply:
   `Welcome` (server state, channels, clients, members, unread counts,
   permissions, ICE servers).
2. Requests `{"id", "op", "d"}` → `{"re", "ok" | "err"}`; events `{"ev", "d"}`.
3. Voice: `voice.offer` (SDP with one sender and 8 receivers) → SDP answer;
   `voice.slot` says whose voice flows through which slot.

## Presence (like Discord)

Being connected is not being in voice. After connecting, `Client.channel` is
`null`: you see everyone and read and write channel chats, but you don't
transmit. `channel.join` enters a channel's voice, `channel.leave` leaves it
(the session stays). A password channel's chat opens once you have entered it
during the session (or with the `channel_join_locked` permission).

Read marks are stored per user in SQLite (`read_marks`), so unread counts
(`Welcome.unread`, `chat.read`) agree on every device; `Welcome.members` lists
known users, offline ones included.

## Identity and permissions

A user is their Ed25519 key (like a TeamSpeak identity): no central accounts,
and it works on any self-hosted server. Groups (Admin, Member) carry permission
lists; one-time tokens grant groups. Channel and server passwords are stored as
Argon2 of TeamSpeak's wire form `base64(sha1(password))`.

## TeamSpeak

- **Official TS3/TS6 clients on our server.** They only accept servers holding
  a TeamSpeak-issued license, so `src/teamspeak/` (cargo feature `teamspeak`)
  installs and supervises the official TeamSpeak 3 server and bridges it:
  - channels and server settings are mirrored one way through ServerQuery
    (`bridge.rs`, `channels.rs`); our core is the authority;
  - TeamSpeak users are *remote sessions* in the core (`core/bridge.rs`):
    visible to everyone, with their state dictated by the TeamSpeak server;
  - one of our users sharing a channel with TeamSpeak users (or chatting with
    one privately) gets a *puppet* on TeamSpeak (`puppet.rs`, tsclientlib).
    Slots are scarce, so everyone else is only listed in the channel
    description (`Presence` in `bridge.rs`, which leaves room for a mixed
    stand-in when the server is full). A user's gated voice goes from the SFU
    straight into their puppet; what puppets hear from TeamSpeak users is
    published to the SFU as the remote session's voice (the first copy of a
    frame wins). No buffering, no transcoding.
- **Our client on TeamSpeak servers.** `vc-client::teamspeak` (tsclientlib)
  presents a TeamSpeak server as `vc/1` (Welcome + events), so the UI has no
  TeamSpeak-specific code; voice goes straight to the native engine.

## Clients

- `apps/web`: React. The connection (`net/connection.ts`) and the voice engine
  (`voice/engine.ts`) sit behind interfaces.
- `apps/desktop`: Tauri 2 with the same UI; voice through the native
  `vc-client` (cpal, Opus with FEC, jitter buffer with PLC), global
  push-to-talk, tray.
