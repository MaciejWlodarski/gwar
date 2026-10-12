# End-to-end encryption (design)

Status: **design** (phase 2). Nothing in this document is implemented. It
records what was agreed, how it fits the existing identities, protocol and
server, and which spikes must pass before building it. [security.md](security.md)
describes what is implemented today.

## Goals

- A server you join cannot read encrypted channels, direct messages,
  server-wide chat, their files or voice. It still routes them, stores
  ciphertext and sees metadata (see "What the server still sees").
- Nobody can be added to an encrypted conversation without every member's
  client seeing it: the list of devices that can read is cryptographic, not the
  server's word.
- Messages keep their author signatures (`gwar message v2`), so a member who
  can decrypt still cannot forge someone else's message.
- TeamSpeak keeps working: channels shared with TeamSpeak stay unencrypted and
  say so.
- Encryption must not cost the latency that is Gwar's main advantage.
- Calm interface: say what is encrypted, never alarm about events that need a
  compromised server or a lost device.

Not goals (for now): hiding metadata from the server; preventing a voice
participant from forging another participant's audio; deniability;
post-quantum suites (MLS can add them later); push notifications.

## Decisions agreed with the owner

- **MLS** ([RFC 9420](https://www.rfc-editor.org/rfc/rfc9420.html)) for
  channel chat, direct messages and server-wide chat; **SFrame**
  ([RFC 9605](https://www.rfc-editor.org/rfc/rfc9605.html)) for voice.
- A channel is **encrypted or not from its creation, for good**. Existing
  channels are never converted. The default is encrypted when the server has
  TeamSpeak compatibility off and plaintext when it is on; the client
  remembers the creator's last choice. Plaintext channels carry a visible
  "not end-to-end encrypted" label.
- Per channel, also fixed at creation: **new members see history** (or not).
- Newcomers **join on their own** (MLS external join) and can write
  immediately. History keys arrive when any member's device is online. There
  is no per-member consent to sharing history: admission is the control.
- Your own history reaches your other devices through Gwar Connect (an option,
  on by default).
- **Private channels** work like Discord's: holders of `channel_edit` manage
  the member list, through a narrow signed chain from the server owner.
  Passwords remain only for plaintext (TeamSpeak) channels.
- The voice key is only for current voice participants.
- Server-wide chat follows the channel rule: both options are chosen when
  the server is set up.
- Encrypted direct messages and server-wide chat are **stored** (as
  ciphertext), so they reach members who are offline and survive a reload.
  Today both are live-only and never stored.
- Pre-release: no compatibility with today's protocol (`vc/3` becomes `vc/4`).

## Building blocks

- **MLS library: OpenMLS** (Rust, 0.9), compiled to WebAssembly for the web
  app. The desktop app runs the same web interface, so one MLS implementation
  serves both; the native voice engine only receives media keys. OpenMLS
  supports external commits and the mandatory suite. Its WebAssembly target is
  built without tests upstream, and external commits are reported to panic
  there (`SystemTime::now`, openmls#1983), so spike S1 decides; the fallbacks
  are mls-rs (WebAssembly builds) and ts-mls (TypeScript, not audited).
- **Ciphersuite** `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519` (0x0001):
  Ed25519 matches every Gwar key.
- **SFrame**: browsers through `RTCRtpScriptTransform` (Chrome/Edge 141+,
  Firefox 117+, Safari 15.4+); the desktop's native engine through the Rust
  `sframe` crate (2.0). Both use the RFC 9605 wire format, so they interoperate.

## Identities in MLS

Each **device** is one MLS member (a leaf). A person with three devices has
three leaves; the interface groups leaves by uid.

The MLS credential binds a leaf to an existing Gwar identity. It is a
`BasicCredential` whose identity is the canonical encoding of:

- `client_signing_pk` and, for Connect accounts, the device certificate (as
  in vc/3; local identities have none);
- `mls_signature_pk`, the key that signs the leaf;
- a binding signature by `client_signing_pk` over
  `gwar mls key v1\n{client_signing_pk}\n{certificate_hash}\n{mls_signature_pk}\n{created_at}`.

A separate MLS signature key is needed because the web device key is a
non-extractable WebCrypto key with an asynchronous API, while MLS libraries
sign synchronously. S1 checks whether the device key can sign directly; if it
can, the binding disappears.

Every client validates every credential itself, with the rules messages use
today: the uid derives from the account key (or the local key), the
certificate is current and not revoked (the client's own Connect feed), the
binding verifies. The server cannot invent a member: a ghost can only be an
identity the attacker really holds, and it is visible in the member list.

- A revoked device, or one whose certificate expired, is removed by the first
  member device that notices (any member may commit the removal).
- Renewing a certificate updates the device's own leaf (an MLS update with
  the new credential).
- Each device updates its leaf at least every seven days, for
  post-compromise security.

## Conversations and groups

One MLS group per encrypted conversation on a server: `channel:{id}`,
`dm:{lower_uid}:{higher_uid}`, `server`. Voice uses a **separate** group per
channel, `voice:{id}`, whose members are only the devices in voice (see
"Voice").

`group_id = SHA256("gwar group v1\n{server_key}\n{conversation}\n{creation_nonce}")`,
with a random nonce chosen by the creator, so a group is bound to one server
and one channel lifetime even if a dishonest server reuses a channel id.

The **server is the delivery service**:

- it stores **key packages** (each device publishes a few per server, plus a
  last-resort one), used to add someone who is offline (a new DM);
- it stores the latest **GroupInfo** of each group, which the committer
  uploads after every commit; newcomers join from it;
- it **orders commits**: the core actor accepts the first commit for an epoch
  and rejects the rest as stale; clients then fetch and retry;
- it keeps the **handshake log** until every member device has caught up, or
  at most 30 days; a device offline longer rejoins by external commit, and
  loses nothing, because history uses stored epoch keys (below);
- it stores **ciphertext** messages and files as it stores messages today.

It can refuse, delay or drop any of these (as it can today). It cannot read
content or add a member unnoticed.

## Who may join

When a device connects, it joins in the background the groups of every
encrypted conversation it may read and is not in yet; in a channel without
history for new members it reads from that moment. Clients check membership
rules before accepting a commit that adds a leaf:

- **Public encrypted channel**: any member of the server. That is the
  server's word, as today for who is on a server; the defence is that every
  reader is listed (channel info shows every device that can read).
- **Private channel**: only uids on the channel's **signed member list**.
- **Direct message**: only devices of the two participants.
- **Server-wide chat**: any member of the server.

The server, as an MLS external sender, proposes removals when someone leaves,
is banned or removed, and when a session ends in a voice group; a member's
client commits them. A server can thus remove people (it can already refuse
them service) but not add them to private channels or DMs.

### Server owner and private channels

Today a server has an Admin group but no owner identity. This design adds one:

- The **owner** is the uid that redeemed the first admin token. `Welcome`
  names it; the client pins it next to the server key pin, and invite links
  carry it, like `server_key`. First contact is trust-on-first-use, as for the
  server key.
- Ownership moves only by a statement signed by the current owner:
  `gwar owner v1\n{server_key}\n{seq}\n{new_owner_uid}\n{signed_at}`.
- The owner signs **grants**:
  `gwar grant v1\n{server_key}\n{seq}\n{uid}\n{add|remove}\n{signed_at}`.
  A granted uid may create private channels and manage their members. The
  owner's client signs a grant when the owner gives a role with
  `channel_edit`; when another admin does so, the grant waits for the owner's
  confirmation ("Allow @name to manage private channels?").
- Each private channel has an append-only **member log**: entries
  `gwar channel member v1\n{group_id}\n{seq}\n{previous_entry_hash}\n{add|remove}\n{uid}\n{actor_uid}\n{grant_seq}\n{signed_at}`,
  signed by the actor's device. An entry counts if the actor was the owner or
  held the grant `grant_seq`, not removed before it. The server stores and
  serves the log; it can withhold entries (a newly added member then waits)
  but not forge them.
- On a removal entry, a member device commits the removal of all leaves of
  that uid.

## Messages

MLS distributes keys; the stored messages use **epoch keys**, so history can
be shared and kept:

- each epoch has `epoch_key = MLS-Exporter("gwar chat v1", group_id, 32)`;
- a message revision uses
  `key = HKDF-SHA256(epoch_key, info = "gwar message key v1" ‖ message_id ‖ revision)`
  and AES-256-GCM with a random nonce;
- the AAD is
  `gwar chat ciphertext v1\n{server_key}\n{conversation}\n{group_id}\n{epoch}\n{message_id}\n{revision}`;
- the plaintext is today's signed message unchanged: the `gwar message v2`
  statement, its signature, certificate and body. After decryption every
  existing check applies (author, conversation, revisions, revocations,
  checkpoints, deletion tombstones).

The server stores `{conversation, message_id, revision, epoch, ciphertext}`
plus the sender's uid from the session, enforces "only the author edits or
deletes" from that uid, and no longer verifies signatures it cannot see; the
clients do. A moderator removal stays an unsigned server removal.

- **Edits** are new revisions encrypted in the current epoch.
- **Attachments**: each file is encrypted with a random key (AES-256-GCM)
  before upload; the key, hash, size, name and type travel in the signed,
  encrypted body. The server stores ciphertext; a file link alone gives only
  ciphertext, whatever `file_access` says.
- **Mentions and unread counts**: the server counts messages per channel
  (metadata it has anyway) but cannot see mentions; clients compute mentions
  and notifications after decrypting.
- **Direct messages** become stored conversations: the first message creates
  the group and adds the other person's devices from their key packages (a
  Welcome stored for them); their other devices join by external commit.

Within an epoch, anyone holding the epoch key can read all of its messages.
That is the cost of stored, shareable history (as with Megolm or Discord);
new epochs come with every membership change and every leaf update.

## History for new members

When the channel allows it, a newcomer receives a **history bundle**: the
epoch keys of earlier epochs, sealed with HPKE to the newcomer's leaf
encryption key (`info = "gwar history v1"`). Any member device that is online
sends it; the server keeps bundles from up to three different members and
drops the rest, so one dishonest member cannot block history. Until one
arrives
the newcomer sees "Earlier messages will appear when another member is
online."

A malicious member could send wrong keys: the affected messages simply do not
open, and since signatures are inside, wrong keys can never make a forged
message verify. Without the option, a newcomer reads from the epoch they
joined.

## Your own devices

Your devices keep the epoch keys of every epoch they were members of. With
the option on (default for Connect accounts), each device also uploads them to
a **key archive** on Gwar Connect: append-only records encrypted with
`HKDF-SHA256(account seed, salt = account public key, info = "gwar archive v1")`,
next to the vault (which is capped at 64 KiB and stays as it is). A new device
reads the archive, so your history comes with you even in channels without
history for new members. Connect sees only the number and size of records.
Serving an older archive only hides keys (messages do not open); it cannot
reveal anything. Local identities have one device and no archive.

## Voice

- The `voice:{id}` group contains the devices in that channel's voice. A
  device entering voice joins it by external commit (the first one creates
  it), checked against the same rules as the channel; the server proposes the
  removal on leave or disconnect and any participant commits it. When the last
  one leaves the group is dropped.
- Each sender's key is
  `MLS-Exporter("gwar voice v1", leaf_index (u32, big-endian), 32)` in the
  current epoch, used as the SFrame base key. The SFrame key id is
  `epoch (low 16 bits) ‖ leaf_index`, so receivers find the key per frame even
  though the SFU moves speakers between the 8 receive slots.
- **Transitions** (as in Discord's DAVE): after a commit, senders switch to the
  new epoch when every participant has acknowledged it or after two seconds;
  receivers keep the previous epoch's keys for ten seconds for frames in
  flight. The SFU stops forwarding to a removed participant at once, so a
  leaver cannot keep listening even during the switch.
- Every participant can derive every sender's key, so a participant could
  forge another's audio. Accepted for now (see Not goals).
- **Web**: `RTCRtpScriptTransform` in a worker encrypts the microphone frames
  and decrypts each receive slot. A browser without it cannot join encrypted
  voice and says so ("Update your browser or use the desktop app").
- **Desktop**: the native engine (`vc-client`) encrypts and decrypts with the
  `sframe` crate. The web layer, which runs MLS, passes the per-epoch sender
  keys through a new `voice_set_keys` command (High risk class: it decides
  what the engine can decrypt).
- **SFU**: forwards the encrypted payload unchanged. Two things change:
  - the packet duration comes from the sender's RTP timestamps instead of the
    Opus header (today `media/opus.rs` reads the first payload byte, which is
    now encrypted);
  - the speech gate keeps using the RFC 6464 audio level header extension,
    which stays in the clear. Who talks, and when, remains visible to the
    server (it already is).
- **TeamSpeak** never receives encrypted voice: encrypted channels are not
  reachable from TeamSpeak (next section).

## TeamSpeak and plaintext channels

- On a server with TeamSpeak compatibility, new channels default to
  plaintext, and server-wide chat (bridged to TeamSpeak server chat) is
  plaintext.
- An encrypted channel still appears on TeamSpeak, so the channel tree stays
  the same, but TeamSpeak users cannot join it (join power above any
  TeamSpeak group). Its description says it is end-to-end encrypted and
  reachable from Gwar.
- No text crosses the bridge from encrypted channels, and direct messages
  with TeamSpeak users stay plaintext (TeamSpeak users have no keys).

## Interface

- Encrypted conversations show a small lock and "End-to-end encrypted" in
  channel info; plaintext channels show "Not end-to-end encrypted" in muted
  text. No banners, no red.
- Channel info lists **who can read**: members and their devices.
- Creating a channel: "End-to-end encrypted" (default per the rule above) and
  "New members see earlier messages", both marked as permanent.
- Messages a device cannot open: "Can't open this message on this device"
  (muted), with the reason in its tooltip.

## What the server still sees

Who is a member of which conversation and voice channel, and their devices;
when messages are sent, their size, and which message is an edit or deletion;
file sizes; who talks and when (audio level); channel names and topics;
everything it sees today about connections (IP addresses, timing). Gwar
Connect additionally sees the size of each account's key archive.

## Protocol changes (sketch)

`vc/4`, with no compatibility with `vc/3`:

- `mls.key_packages.publish`, `mls.key_packages.claim {uid}`;
- `mls.group_info {group}` and its upload with every commit;
- `mls.commit {group, epoch, commit, welcome?, group_info}`, answered with
  ok or `stale`; event `mls.commit`;
- `mls.history.share {group, leaf, bundle}`, event `mls.history`;
- `owner.*`, `grant.*`, `channel.members.*` for the signed statements above;
- `chat.send`/`chat.edit` carrying `{epoch, ciphertext}` in encrypted
  conversations; `Channel` gains `encrypted`, `history`, `private`;
- `voice.epoch.ack` for voice transitions;
- Connect: `/v1/archive` (append, list since cursor).

## Spikes

Throwaway code on a branch; only results come back into this document.

| Spike | Question | Passes when |
| --- | --- | --- |
| S1 MLS in the web app | Does OpenMLS 0.9 work in WebAssembly for create, external commit, add/remove, exporter, in Chrome, Firefox and Safari? Bundle size? Speed? How to persist its state in IndexedDB? Can the WebCrypto device key sign MLS leaves? | External commit works in all three; gzip size acceptable (target under 400 KB); a commit in a 100-device group under 100 ms on a laptop; state survives reloads. Otherwise mls-rs or ts-mls is evaluated the same way. |
| S2 SFrame in browsers | Does `RTCRtpScriptTransform` encrypt Opus frames to our SFU and decrypt across slot changes, in Chrome, Firefox and Safari? CPU and latency added? | Clean audio in all three; under 1 ms per frame; speakers moving between slots keep decrypting. |
| S3 SFrame native | Does `sframe` 2.0 in `vc-client` interoperate with S2? | Frames from the browser decrypt natively and back, with shared test vectors. |
| S4 SFU and latency | Forward encrypted frames with timestamp-based durations; does the speech gate still work; what does encryption add end to end? | Talking indicators and slots unchanged; mouth-to-ear latency within 2 ms of plaintext on the VM. |

## Open questions

- Grants given by an admin other than the owner wait for the owner. Is that
  acceptable on servers whose owner is rarely online, or should the owner be
  able to delegate granting?
- History bundles grow with every epoch (about 40 bytes each). If channels
  reach many thousands of epochs, a key tree can replace the flat list.
- SFrame suite for voice: `AES_128_GCM_SHA256_128` (16-byte tag) by default;
  S4 measures whether a shorter-tag suite is worth it for bandwidth.
