# End-to-end encryption (design)

Status: **design** (phase 2). Nothing in this document is implemented. It
records what was agreed, how it fits the existing identities, protocol and
server, and which spikes must pass before building it. [security.md](security.md)
describes what is implemented today.

## Goals

- A server you join cannot read encrypted channels, direct messages,
  server-wide chat, their files or voice. It still routes them, stores
  ciphertext and sees metadata (see "What the server still sees").
- Every client checks who can decrypt: the list of devices in an encrypted
  conversation is cryptographic, not the server's word. In a private channel
  or a direct message, the server cannot add anyone.
- Messages keep author signatures, so a member who can decrypt still cannot
  forge someone else's message.
- TeamSpeak keeps working: channels shared with TeamSpeak stay unencrypted and
  say so, and nothing from an encrypted channel ever reaches the bridge.
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
  the member list, under the server owner. Passwords remain only for
  plaintext (TeamSpeak) channels.
- The voice key is only for current voice participants.
- Server-wide chat follows the channel rule; both options are chosen when the
  server is set up.
- Encrypted direct messages and server-wide chat are **stored** (as
  ciphertext), so they reach members who are offline and survive a reload.
  Today both are live-only and never stored.
- Pre-release: no compatibility with today's protocol (`vc/3` becomes `vc/4`).

## Building blocks

- **MLS library: OpenMLS** (Rust, 0.9), compiled to WebAssembly for the web
  app. The desktop app runs the same web interface, so one MLS implementation
  serves both; the native voice engine only receives media keys. OpenMLS
  supports external commits, external senders and the mandatory suite. Its
  WebAssembly target is built without tests upstream, and external commits are
  reported to panic there (`SystemTime::now`, openmls#1983), so spike S1
  decides; the fallbacks are mls-rs (WebAssembly builds) and ts-mls
  (TypeScript, not audited).
- **Ciphersuite** `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519` (0x0001):
  Ed25519 matches every Gwar key.
- **SFrame**: browsers through `RTCRtpScriptTransform` (Chrome/Edge 141+,
  Firefox 117+, Safari 15.4+); the desktop's native engine through the Rust
  `sframe` crate (2.0). Both use the RFC 9605 wire format, so they
  interoperate.

All new statements follow the existing rules: UTF-8, `\n` separators, no
trailing newline, canonical unpadded base64url for binary fields, decimal
Unix milliseconds. Derived keys are 32 bytes; AES-256-GCM uses random 96-bit
nonces and 128-bit tags.

## Identities in MLS

Each **device** is one MLS member (a leaf), identified by its
`client_signing_pk`, the key it already uses for vc/3 and message
signatures. A person with three devices has three leaves; the interface
groups leaves by uid.

- The **leaf signature key is the device key itself**. MLS signs with its own
  labels (`MLS 1.0 …`), which no Gwar statement starts with. The key is
  stored as a JWK today and only imported as non-extractable for WebCrypto,
  so the MLS library can sign with it; spike S1 confirms this. Should it
  fail, a separate MLS key bound by a device-key signature is the fallback.
- The credential is a `BasicCredential` whose identity is the canonical
  encoding of the device certificate (Connect accounts) or `-` and the key
  (local identities).

Every client validates every leaf it accepts, on **every** path that brings
one in: the tree of a Welcome or GroupInfo it joins from, and the tree that
results from every commit (adds, updates, external commits, path updates):

- the leaf signature key equals the device key in the credential, and the
  encoding is canonical;
- the uid derives from the account key (or the local key);
- the certificate is current and not revoked, by the client's own Connect
  feed (a stale feed is used as it is, as for messages today; it cannot be
  forced fresh);
- one leaf per device key, and an update keeps the leaf's uid and device key;
- the resulting membership satisfies the conversation's rule (below).

Historical checks on old messages may accept an expired or revoked device
with a qualification; membership never does. A revoked or expired leaf is
removed by the first member device that notices. Renewing a certificate is an
MLS update of the device's own leaf. Each device commits an update of its
leaf at least every seven days while it is online; an overdue device updates
before sending.

## Conversations and groups

One MLS group per encrypted conversation on a server: `channel:{id}`,
`dm:{lower_uid}:{higher_uid}`, `server`. Voice uses a **separate** group per
channel, `voice:{id}` (see "Voice").

### The descriptor

Each group carries an immutable **descriptor** in a required GroupContext
extension, set by its creator:

```text
gwar conversation v1
{server_key}
{conversation}
{kind}              channel | dm | server | voice
{private}           0 | 1
{history}           0 | 1  (new members see history)
{owner_uid}         private channels; - otherwise
{creator_uid}
{created_at}
{creation_nonce}
```

`group_id = SHA256(descriptor)`. Clients reject any commit that changes the
descriptor, and take `private` and `history` only from it, never from server
metadata. Each client **pins** the group id per conversation the first time
it joins; later it accepts only that group. A channel deleted and recreated
gets a new id, so it is a new conversation.

For a direct message, the server keeps one group per pair: it accepts the
first creation and answers later attempts with the existing group, which the
loser joins. A dishonest server could give the two people different groups;
that only keeps them apart, since only their devices can be in either.

### The server as delivery service

- It stores **key packages** per device (a few, plus a last-resort one),
  used to add someone who is offline, as in a new direct message.
- It stores the latest **GroupInfo** of each group, with the `external_pub`
  and ratchet tree extensions, uploaded by the committer with every commit;
  newcomers join from it.
- Every group lists the server's key in its `external_senders` extension.
  The server may only propose **removals** that way (when someone leaves the
  server, is banned or removed, or leaves voice); clients ignore anything
  else it proposes, and a member's client commits.
- It **orders commits**: the core actor accepts the first commit for an epoch,
  stores it atomically with its GroupInfo and any Welcome, and answers the
  rest `stale`. Clients process epochs strictly in order, merge their own
  pending commit only after the server accepted it, and otherwise discard it,
  catch up and retry. An acceptance never skips local validation.
- It keeps the **handshake log** (commits and the proposals they reference)
  for 90 days, the longest a device certificate lasts. A device offline
  longer rejoins by external commit.
- It stores **ciphertext** messages and files.

### Freshness

Clients remember the highest epoch they saw per group and never accept a
GroupInfo or tree behind it, or a fork. A new device has no such memory: a
dishonest server could show it an old GroupInfo that still contains a since
removed device, and the newcomer's first messages would then be readable by
that device until the next commit reaches it. security.md will state this
first-contact limit. "Every client sees who can decrypt" means each client
validates the membership it uses; a dishonest server can still delay
delivery to some clients.

### Joining

When a device connects, it joins in the background, a few at a time with
jittered retries, the groups of every encrypted conversation it may read and
is not in yet. In a channel without history for new members it reads from
that moment. Rules on the resulting membership of every commit:

- **Public encrypted channel, server-wide chat**: devices of any member of the
  server. That is the server's word, as who is on a server is today; the
  defence is that every reader is listed (channel info shows every device that
  can read). A dishonest server could also hand a newcomer a separate group of
  its own for the same channel; that newcomer's list then shows who really
  reads.
- **Private channel**: only devices of uids on the channel's member list
  (next section).
- **Direct message**: only devices of the two participants.

### Removal

When a client learns that a leaf must go (a revoked or expired device, a
member removed from a private channel, a server removal proposal), it
**stops sending and sharing history** in that group until an accepted epoch
no longer contains the leaf, and commits the removal itself if it may. If
another commit wins first, it recomputes what still has to be removed from
the resulting tree and retries.

## Private channels

Today a server has an Admin group but no owner identity. This design adds one
and keeps everything else inside the private channel's own MLS group, so the
group's epochs order every change:

- The **owner** is the uid that redeemed the first admin token. `Welcome`
  names it; the client pins it per server key (not per endpoint), next to the
  server key pin, and invite links carry it, like `server_key`. First contact
  without a pin or an invite is trust-on-first-use: a dishonest server could
  nominate its own owner to a fresh client, exactly as it could present its
  own key. A pinned owner changes only through the owner's own commits (next
  points); a different owner on a later visit shows the same neutral notice
  as a changed handle.
- A private channel's GroupContext holds a second extension, the **access
  list**: `owner_uid`, `managers` (uids) and `members` (uids). It changes only
  by commits: `managers` and `owner_uid` only by a device of the owner,
  `members` by a device of the owner or of a manager, judged by the list of
  the previous epoch. Every commit's resulting tree may contain only devices
  of uids on the list, so removing a member and removing their devices is one
  commit.
- Managers mirror `channel_edit`: when the owner gives a role with
  `channel_edit`, the owner's client adds that uid as a manager in every
  private channel. When another admin does it, the server reports a pending
  manager and the change waits until the owner's client confirms it ("Allow
  @name to manage private channels?").
- Owner transfer is the owner's commit naming the new owner in every private
  channel. If the owner's keys are lost for good, existing private channels
  keep their lists; new ones are created under a new owner, which members see
  as the notice above.

## Messages

MLS distributes keys; the stored messages use **epoch keys**, so history can
be shared and kept:

- each epoch has `epoch_key = MLS-Exporter("gwar chat v1", group_id, 32)`;
- each message revision uses
  `key = HKDF-SHA256(epoch_key, info = "gwar message key v1" ‖ message_id ‖ revision (u32, big-endian))`
  and AES-256-GCM;
- the AAD is
  `gwar chat ciphertext v1\n{server_key}\n{conversation}\n{group_id}\n{epoch}\n{message_id}\n{revision}`;
- the plaintext is a signed message, statement **`gwar message v3`**: today's
  v2 fields plus `{group_id}` and `{epoch}` (both `-` in plaintext
  conversations), so the author's signature binds the message to the group
  lifetime and epoch, not only to the conversation name. Clients check that
  every field repeated inside and outside the ciphertext is equal, then apply
  every existing check (author, revisions, revocations, checkpoints).

The server stores `{conversation, message_id, revision, epoch, ciphertext,
file ids}` and the sender's uid from the session. It enforces "only the
author edits or deletes" with that uid and no longer checks signatures it
cannot see; clients do. Plaintext channels keep today's server-side checks
with v3 statements.

- **Edits** are new revisions, encrypted in the current epoch.
- **Author deletion** is an explicit outer operation, `chat.delete_own
  {message_id}`, so the server can delete the files and refuse further
  edits, plus the signed deletion revision inside, encrypted in the current
  epoch. A device that cannot open the current epoch still sees the outer
  deletion and shows a muted "deleted" line; with the key it shows the
  verified author deletion as today. A moderator's removal stays an unsigned
  server removal.
- **Attachments**: a file is encrypted with a random key (AES-256-GCM)
  before upload, under an opaque name and `application/octet-stream`; the
  server checks the ciphertext hash and size as it checks plaintext today.
  The signed body (body v3) carries, per attachment: plaintext hash and size,
  name, type, the file key, and the ciphertext hash and size. Bytes are
  checked against the ciphertext hash, decrypted, then checked against the
  plaintext hash before display. The outer file ids tie files to the message
  for cleanup and `file_access`; a link alone gives only ciphertext.
- **Mentions and unread counts**: the server counts messages per
  conversation, and keeps read marks for direct messages as for channels, but
  cannot see mentions. Clients compute mentions after decrypting and
  verifying; a key arriving later never notifies twice.
- **Direct messages** become stored and routed by uid: the first message
  creates the group and adds the other person's devices from their key
  packages (a Welcome stored for them, the last-resort package if they ran
  out); their other devices join by external commit.

Within an epoch, anyone holding the epoch key can read all of its messages,
and devices keep old epoch keys for history: retained history gives up
forward secrecy for those epochs, as in any stored, shareable history.
New epochs come with every membership change and every leaf update.

## History for new members

When the channel's descriptor allows it, a newcomer receives a **history
bundle**: earlier epoch keys, each tagged with group id and epoch, sealed
with HPKE (RFC 9180) to the newcomer's leaf encryption key and signed by the
sender's device key over the server key, group id, recipient device key and
leaf key, and the list of epochs. Any member device that is online sends one.
The server keeps bundles from up to three different uids and drops the rest.
The newcomer merges what they cover, asks again for epochs still missing,
and on a conflict keeps the key it derived itself, or else drops that epoch.
Until a bundle arrives it shows "Earlier messages will appear when another
member is online."

The same mechanism lets a **returning member** catch up on epochs it missed
(its devices were all offline past the handshake log, or it was away): a
sharer includes only epochs in which, by its own records, that uid had a
device in the group. In a channel without history for new members, that is
all a returning member gets. A key nobody kept is lost for good, and the
messages of that epoch stay unreadable on that device.

Signatures are inside the ciphertext, so a wrong key can never make a forged
message verify. AES-GCM does not commit to its key, so a malicious sharer
could in theory make one ciphertext open differently under a different key;
the message then still needs a valid author signature, and keys derived
locally always win.

## Your own devices

Your devices keep the epoch keys of every epoch they were members of, and
those they received in bundles. With the option on (default for Connect
accounts), each device also uploads them to a **key archive** on Gwar
Connect, next to the vault (which is capped at 64 KiB and stays as it is):

- Records are encrypted with an **archive key** that is random, not derived
  from the account seed, and has numbered generations. Each generation is
  sealed to every current device's X25519 key (published, signed by the
  device key, with the device record). When a device is revoked, a remaining
  device creates the next generation for the remaining devices; devices
  remember the highest generation they saw and never go back.
- Each record is AEAD-encrypted with AAD binding account key, server key,
  group id, epoch and generation; imports only add keys and never overwrite.
- A new device reads the archive, so your history comes with you, also in
  channels without history for new members.
- An older or partial archive can only hide keys, and so hide newer revisions
  or deletions on that device; it cannot reveal anything. It never restores
  MLS state, membership or the current generation.

Local identities have one device and no archive.

## Voice

### Keys

- The `voice:{id}` group contains the devices in that channel's voice. A
  device entering voice joins it by external commit (the first one creates
  it), under the same rules as the channel; the server proposes its removal on
  leave or disconnect, and any participant commits. The server maps each leaf
  to the session it joined from. When the last one leaves, the group is
  dropped.
- Each sender's key is
  `MLS-Exporter("gwar voice v1", leaf_index (u32, big-endian), 32)` in the
  current epoch, used as the SFrame base key. The SFrame key id is
  `epoch (low 32 bits) ‖ leaf_index (32 bits)`, so receivers find the key per
  frame even though the SFU moves speakers between the 8 receive slots.
  Replay windows are kept per key.
- One encryptor owns each sender key and its SFrame counter. An engine that
  restarts, or cannot prove its counter continued, does not reuse the key: it
  commits an update of its leaf first, which gives everyone a new epoch.
- Every participant can derive every sender's key, so a participant could
  forge another's audio. Accepted for now (see Not goals).

### Epoch changes

- An epoch change is identified by `(group_id, epoch)`. A participant
  acknowledges it (`voice.epoch.ack`) only once the new keys are installed in
  its actual decryptor (worker or native engine).
- **Removal**: a sender switches to the new epoch as soon as it has
  processed the commit, without waiting. A removed participant gets nothing
  under keys it does not have, whatever the SFU does; the SFU also stops
  forwarding to it.
- **Addition**: senders switch when every remaining participant has
  acknowledged, or after two seconds, so the newcomer is heard and hears
  quickly without anyone missing frames.
- Receivers keep keys of at most three earlier epochs, each for ten seconds,
  for frames in flight. Stale or out-of-order acknowledgements are ignored.

### Clients

- **Fail closed**: no frame is sent or played without encryption; while keys
  are missing the sender sends nothing and receivers drop frames. The
  interface shows "Securing voice…" until keys are active.
- **Web**: `RTCRtpScriptTransform` in one worker; the sender transform and
  all eight receiver transforms are attached when the transceivers are
  created, before any frame flows. Keys are posted to the worker only when
  they change.
- **Desktop**: the native engine (`vc-client`) encrypts and decrypts with the
  `sframe` crate, decrypting before it inspects Opus or fills the jitter
  buffer. The web layer, which runs MLS, passes keys through a new
  `voice_set_keys` command (High risk class): bound to the engine run,
  server, session, voice group and epoch, with receive keys installed
  separately from sender activation, and ignored after the run ends.
- **Frame size**: Opus frames are kept small (20 ms at the current bitrates
  is about 120–200 bytes, plus at most 33 bytes of SFrame); clients cap an
  encoded frame at 1000 bytes so a frame is never split across RTP packets.

### SFU

- It forwards the encrypted payload unchanged.
- **Timing**: today a slot's RTP time advances by the previous packet's
  duration, read from the Opus header, which is now encrypted. Instead, while
  the same speaker stays on a slot, the forwarded time advances by the
  difference of the sender's own RTP timestamps (which already accounts for
  losses and silence), forwarded at once; on a speaker change or a gap, the
  current elapsed-time rule applies. The native client's timestamps must
  advance per frame, not per wall-clock interval, for this to hold.
- **Speech gate**: unchanged, from the RFC 6464 audio level header extension,
  which stays in the clear. Who talks, and when, remains visible to the
  server, as it is today. Rewritten RTP headers are not part of SFrame's
  authenticated data.

## TeamSpeak and plaintext channels

- On a server with TeamSpeak compatibility, new channels default to
  plaintext, server-wide chat (bridged to TeamSpeak server chat) is
  plaintext, and the default channel is always plaintext.
- An encrypted channel still appears on TeamSpeak, so the channel tree stays
  the same, but TeamSpeak users cannot join it (join power above any
  TeamSpeak group); its description says it is end-to-end encrypted and
  reachable from Gwar.
- The bridge never carries anything of an encrypted channel, enforced where
  audio and text enter and leave it, not only by channel permissions: the SFU
  feeds no puppet audio from an encrypted channel (also during a puppet's
  grace period or a refused move), whispers from TeamSpeak cannot target
  encrypted voice, and no channel text crosses in either direction.
- Direct messages with TeamSpeak users stay plaintext and labeled; a
  puppet kept for them carries text only, never audio from an encrypted
  channel.

## Interface

- Encrypted conversations show a small lock and "End-to-end encrypted" in
  channel info; plaintext channels, plaintext server chat and plaintext
  direct messages show "Not end-to-end encrypted" in muted text. No banners,
  no red.
- Channel info lists **who can read** (devices in the chat group) and,
  separately, **who can hear** (devices in the voice group right now).
- Creating a channel: "End-to-end encrypted" (default per the rule above),
  "New members see earlier messages" and, for managers, "Private"; marked as
  permanent.
- Messages a device cannot open: "Can't open this message on this device"
  (muted), with the reason in its tooltip.
- A browser without encoded transforms says "Update your browser or use the
  desktop app to join encrypted voice."

## What the server still sees

- Who is a member of which conversation and voice channel, their devices and
  uids (global, so linkable across servers), every membership change and
  epoch, and who sends each message to which conversation (including who
  talks privately with whom).
- When messages are sent, their size, edits, deletions and read marks; file
  sizes and upload and download times; who sends history bundles to whom, and
  their size.
- Who talks and when (audio level); channel names and topics.
- Everything in plaintext channels, plaintext server chat and direct messages
  with TeamSpeak users.
- Everything it sees today about connections (IP addresses, timing).

Gwar Connect additionally sees which devices use the key archive, when, from
which addresses, and the number and size of records.

## Protocol changes (sketch)

`vc/4`, with no compatibility with `vc/3`:

- `mls.key_packages.publish`, `mls.key_packages.claim {uid}`;
- `mls.group_info {group}`, `mls.commit {group, epoch, commit, welcome?,
  group_info}` answered with ok or `stale`, event `mls.commit`;
- `mls.history.share {group, recipient, bundle}`, `mls.history.request`,
  event `mls.history`;
- `dm.open {uid}` returning the pair's group (creating it once);
- `owner` in `Welcome`, pending managers, owner and manager confirmation;
- `chat.send`/`chat.edit` carrying `{epoch, ciphertext, file_ids}` in
  encrypted conversations, `chat.delete_own`; message statement v3 and body v3
  everywhere; `Channel` gains `encrypted`, `history`, `private`;
- `voice.epoch.ack`;
- Connect: `/v1/archive` (append, list since cursor), archive generations,
  device X25519 keys.

## Spikes

Throwaway code on a branch; only results come back into this document.
Measurements name the hardware and browser versions and report medians and
p95/p99.

| Spike | Question | Passes when |
| --- | --- | --- |
| S1 MLS in the web app | Does OpenMLS 0.9 work in WebAssembly for create, external commit, add/remove, external-sender removals, exporter, GroupContext extensions, in Chrome, Firefox and Safari? Can it sign with the existing device key? Bundle size, speed, persistence in IndexedDB? | All operations work in the three browsers; gzip size under 400 KB; a commit in a 100-device group under 100 ms (p95) on a laptop; state survives reloads and a crash mid-commit. Otherwise mls-rs or ts-mls is evaluated the same way. |
| S2 SFrame in browsers | Does `RTCRtpScriptTransform` encrypt to our SFU and decrypt across slot changes in Chrome, Firefox and Safari, failing closed? | Clean audio with eight senders; speakers moving between slots keep decrypting; added latency p99 under 2 ms including worker queueing, also with the tab in the background; no plaintext frame ever sent or played. |
| S3 SFrame native | Does `sframe` 2.0 in `vc-client` interoperate with S2? | Shared test vectors pass both ways; frames at the 1000-byte cap cross the real SFU intact in both directions. |
| S4 SFU and latency | Timestamp-based forwarding, speech gate, end-to-end cost. | Talking indicators and slots unchanged; mouth-to-ear p95 within 2 ms of plaintext on the VM; no change with packet loss and silence. |
| S5 Churn | Joins, removals, restarts, lost acknowledgements, withheld commits, repeated key installation, leaf reuse, epoch wrap, stale `voice_set_keys` calls. | Zero nonce reuse; a removed participant decrypts nothing after the removal commit is processed; no plaintext fallback. |
| S6 TeamSpeak isolation | With the official TS3 server: moves, whispers, puppets in their grace period, query listeners, restarts. | Zero bytes of an encrypted channel's audio or text reach TeamSpeak. |

## Open questions

- A manager added by an admin other than the owner waits for the owner's
  client. Is that acceptable on servers whose owner is rarely online, or
  should the owner be able to delegate that confirmation?
- History bundles grow with every epoch (about 40 bytes each). If channels
  reach many thousands of epochs, a key tree can replace the flat list.
- SFrame suite for voice: `AES_128_GCM_SHA256_128` (16-byte tag) by default;
  S4 measures whether a shorter-tag suite is worth it for bandwidth.
