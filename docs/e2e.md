# End-to-end encryption (design)

Status: **design** (phase 2); being built in phase 4, see
[e2e-build.md](e2e-build.md) for the order and the exact encodings. It
records what was agreed, how it fits the existing identities, protocol and
server, and what the spikes measured (see "Spikes"). [security.md](security.md)
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
  serves both; the native voice engine only receives media keys. Spike S1
  ran every operation this design needs in Chrome, Firefox and WebKit; the
  crate's `js` feature avoids the external-commit panic of openmls#1983.
  mls-rs passed too and is smaller (267 KB against 447 KB gzip), but OpenMLS
  is chosen: its core had an independent security audit (SRLabs, 2025–2026),
  and Wire and XMTP run it in browsers, while mls-rs has had no full
  third-party audit. The audit left out the crypto and storage providers,
  and Wire and XMTP use their own integrations, so the provider Gwar uses
  (`openmls_rust_crypto`) and our storage get their own review before
  release. The module is loaded on first use, not with the page. Its size
  budget, first set at 400 KB gzip, is raised to 450 KB, which leaves little
  room: further size work belongs to the implementation.
- **Ciphersuite** `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519` (0x0001):
  Ed25519 matches every Gwar key.
- **SFrame**: browsers through `RTCRtpScriptTransform` (Chrome/Edge 141+,
  Firefox 117+, Safari 15.4+) with a small RFC 9605 implementation on
  WebCrypto in the worker, not audited, which gets its own review before
  release; the
  desktop's native engine through the Rust `sframe` crate (2.0). They
  interoperate (S3). SFrame application metadata is always empty: `sframe`
  2.0 authenticates metadata before the header, the reverse of RFC 9605,
  which only differs when metadata is present. Clients reject non-empty
  metadata, and a test keeps the RFC vector that fails today until it is
  fixed upstream ([sframe-rs#124](https://github.com/TobTheRock/sframe-rs/issues/124)).

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
  so the MLS library signs with its seed; spike S1 confirmed this, and no
  separate MLS key is needed.
- The credential is a `BasicCredential` whose identity is the canonical
  encoding of the device certificate (Connect accounts) or `-` and the key
  (local identities).

Every client validates every leaf it accepts, on **every** path that brings
one in: the tree of a Welcome or GroupInfo it joins from, and the tree that
results from every commit (adds, updates, external commits, path updates):

- the leaf signature key equals the device key in the credential, and the
  encoding is canonical;
- the uid derives from the account key (or the local key);
- a leaf the commit introduces or changes has a certificate covering the
  commit's time (signed by its committer); revocations and certificates that
  ran out later never make a client reject a commit, so clients with
  different Connect feeds follow the same chain. Instead a client does not
  send in a group while it has a leaf that is not current now by its own
  feed (a stale feed is used as it is, as for messages today), and commits
  its removal (see "Removal");
- one leaf per device key, and an update keeps the leaf's uid and device key;
- the resulting membership satisfies the conversation's rule (below).

Historical checks on old messages may accept an expired or revoked device
with a qualification; current membership never does: a tree that holds such a
leaf after catching up blocks sending until the leaf is removed, which the
first member device that notices does. Renewing a certificate is an
MLS update of the device's own leaf. Each device commits an update of its
leaf at least every seven days while it is online; an overdue device updates
before sending.

## Conversations and groups

One MLS group per encrypted conversation on a server: `channel:{id}`,
`dm:{lower_uid}:{higher_uid}`, `server`. Voice uses a **separate** group per
call in a channel (see "Voice").

### The descriptor

Each group carries an immutable **descriptor** in a required GroupContext
extension, with its creator's device signature:

```text
gwar conversation v1
{server_key}
{conversation}
{kind}              channel | dm | server | voice
{incarnation}       0, then +1 per restart (below)
{previous_group_id} the incarnation it replaces; - for 0
{private}           0 | 1
{history}           0 | 1  (new members see history)
{founding_owner}    private channels; - otherwise
{channel_group_id}  voice: the channel's group; - otherwise
{creator_uid}
{created_at}
{creation_nonce}
```

`group_id = SHA256(descriptor)`. Clients reject any commit that changes the
descriptor, and take `private` and `history` only from it, never from server
metadata. The creator must be allowed to create it: the owner or a manager
for a private channel (checked as in "Private channels"), a participant for a
direct message, a member of the channel group for a voice call, and any
member of the server for a public channel or server-wide chat (the server's
word, as membership itself is).

Each client **pins** the group id per conversation and incarnation the first
time it joins, and afterwards accepts only that group. A channel deleted and
recreated gets a new channel id, so it is a new conversation. A conversation
is **restarted** (next incarnation) only when its group cannot continue, for
example because the server lost its state. A successor names its
predecessor, keeps its `kind`, `private`, `history` and `founding_owner`,
and must come from someone allowed to create the predecessor; a client that
missed several restarts follows the chain from the incarnation it pinned.
Clients keep the highest accepted incarnation as the live one and show a
quiet "This conversation was restarted" line; earlier incarnations stay
readable with the keys a device has, but nobody writes to them. Voice groups
are pinned only for the call (see "Voice").

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

Clients remember, per group, the highest epoch they saw and the hash of its
GroupContext (which commits to the tree and the transcript), and never accept
a GroupInfo or tree behind it, or a different GroupContext for the same epoch
(a fork). After a gap they catch
up through the handshake log, which proves the chain; a GroupInfo alone does
not, so a device that rejoins by external commit after a gap treats it as a
first contact. A new device has no such memory: a
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
that moment. Permission to be in a group is separate from permission to
change its rules (the access list below). Rules on the resulting membership
of every commit:

- **Public encrypted channel, server-wide chat**: devices of any member of the
  server. That is the server's word, as who is on a server is today; the
  defence is that every reader is listed (channel info shows every device that
  can read). A dishonest server could also hand a newcomer a separate group of
  its own for the same channel; that newcomer's list then shows who really
  reads.
- **Private channel**: only devices of the owner, the managers and the uids
  on the channel's member list (next section).
- **Direct message**: only devices of the two participants.

### Removal

Catching up and being allowed to send are separate. A client processes every
valid commit in order, judging it by the rules **as they stood in that
epoch** (the access list then, and certificates at the commit's time for
the leaves it brings in), even when the resulting
tree holds a leaf that should go (an offline client catching up, or a
competing commit by a client with a staler revocation feed). Replaying
history never lowers what the client already knows to be current (its
checkpoints below). But while the current tree breaks a current rule, it
**stops sending and sharing history** in that group, commits the removal
itself, and if another commit wins first, recomputes what still has to go
and retries. Removal needs a member's commit; if no member device can commit
any more (all their state lost), the conversation is restarted (above).

## Private channels

Today a server has an Admin group but no owner identity. This design adds one,
with signed statements that newcomers can check without any history:

- The **owner** is the uid that redeemed the first admin token. `Welcome`
  names it; the client pins it per server key (not per endpoint), next to the
  server key pin, and invite links carry it, like `server_key`. First contact
  without a pin or an invite is trust-on-first-use: a dishonest server could
  nominate its own owner to a fresh client, exactly as it could present its
  own key.
- **Ownership moves** only by a chain of statements, each signed by the
  previous owner's **account key** (a local identity's own key) and naming
  the hash of the previous link:
  `gwar owner v1\n{server_key}\n{seq}\n{previous_link_hash}\n{previous_owner_uid}\n{new_owner_uid}\n{signed_at}`.
  Moving ownership asks for the password, as revoking a device does, so a
  lost or revoked device cannot sign a link, and a link is checked by its
  signature alone. The server serves the chain; a client accepts a chain
  only if it extends the head it pinned. If the owner's account is lost for
  good (password and recovery code) there is no link, and a new owner shows
  the same neutral notice as a changed handle, which the user can accept.
- The **managers** of the server's private channels are a list signed by a
  device of the current owner:
  `gwar managers v1\n{server_key}\n{seq}\n{owner_uid}\n{manager uids}\n{signed_at}`.
  Managers mirror `channel_edit`: when the owner gives a role with
  `channel_edit`, the owner's client signs a new list. When another admin
  does it, the server reports a pending manager, and the list waits until the
  owner's client confirms it ("Allow @name to manage private channels?").
- Each private channel's GroupContext holds the **access list**: the owner
  chain head, the manager list, and the members:
  `gwar channel members v1\n{group_id}\n{seq}\n{member uids}\n{signer_uid}\n{signed_at}`,
  signed by a device of the current owner or of a manager on that manager
  list. When the owner removes a manager or ownership moves, the owner's
  client re-signs the member lists that no longer qualify, in the same
  commits.
- Clients **checkpoint** `(seq, statement hash)` for the owner chain head and
  the manager list per server, and for each member list. A lower `seq` is
  rejected; the same `seq` is accepted only with the same content, so two
  devices signing different lists at the same number cannot be swapped later.
  Any member may commit a newer signed list (with the removal of leaves it
  requires, in the same commit). A newcomer checks the signatures, so a
  dishonest server cannot invent a list; it can only show an older one the
  newcomer has not seen superseded, the same first-contact limit as under
  "Freshness".
- **Who signed, and when.** The manager list and member lists are checked
  like messages: the signature and the certificate chain to the right uid.
  Each is a full list, so only the newest one grants authority, and only
  while it is **current**: its signer's certificate is current and not
  revoked. A client moves a checkpoint only to a list that is current when
  it first sees it; a newer list from a since-revoked or expired device is
  ignored, and the next current list supersedes it. Members keep the last
  list they accepted; newcomers need a current one. Older lists matter only
  for replaying past epochs, which never grants current authority: sending
  and history sharing follow the current tree and the current lists. So
  that a current list always exists, the owner's and managers' clients
  re-sign the manager list and member lists at least every 60 days and
  before their certificates expire; if no owner or manager has been active
  for 90 days, newcomers cannot join that private channel until one
  returns. The descriptor's own signature is history: its authority for
  newcomers comes from the current access list.
- The owner and the managers are always allowed readers, as admins see every
  channel on Discord.

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
with v3 statements; their body v3 attachment entries have `-` for the file
key and the ciphertext hash and size, and keep today's upload checks.

- **Edits** are new revisions, encrypted in the current epoch.
- **Author deletion** is an explicit outer operation, `chat.delete_own
  {message_id}`, sent together with the signed deletion revision encrypted in
  the current epoch; the server stores both atomically, deletes the files and
  refuses further edits. The author's device needs the current epoch for
  that, so a device still catching up deletes once it has. A device that
  cannot open the deletion revision shows a muted "deleted" line, which is
  the server's report, never a verified author deletion; with the key it
  shows the verified author deletion as today. A moderator's removal stays an
  unsigned server removal.
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
with HPKE (RFC 9180) to the newcomer's **device HPKE key**, an X25519 key each
device publishes signed by its device key (not the leaf encryption key, which
changes with every update and whose old private halves OpenMLS deletes). The
sender's device key signs the whole bundle: server key, group id, recipient
device key and HPKE key, the HPKE encapsulation and the ciphertext. The newcomer accepts
it only from a device that is a member of the group in its current epoch.
Any member device that is online sends one.
The server keeps bundles from up to three different uids and drops the rest.
The newcomer merges what they cover, asks again for epochs still missing,
and on a conflict keeps the key it derived itself, or else drops that epoch.
Until a bundle arrives it shows "Earlier messages will appear when another
member is online."

The same mechanism lets a **returning member** catch up on epochs it missed
(its devices were all offline past the handshake log, or it was away): a
sharer includes only epochs in which, by its own records, that uid had a
device in the group, and shares nothing where it has no such record; holding
a key is not evidence. In a channel without history for new members, that is
all a returning member gets; in a channel with history, it gets what any
newcomer gets. A key nobody kept is lost for good, and the
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
  from the account seed, and has numbered generations. Each device publishes
  an X25519 key signed by its device key, with its device record. The
  **recovery key** is an X25519 key derived from the recovery code; its
  public half is published with a version, signed by the account key, when
  the recovery code is set.
- A generation is created by a device of the account with a current
  certificate, as a **manifest** signed by its device key: account key,
  generation, hash of the previous manifest, recipients, recovery key
  version, the account's signed revocations (`gwar revoke v1`) its creator
  knows, and the generation key sealed (HPKE) to each recipient and to the
  recovery key. Connect accepts only the first manifest for each generation
  number. Devices checkpoint `(generation, manifest hash)` and reject a
  different manifest for a number they saw, and learn every revocation a
  manifest carries.
- **Uploading** uses the newest generation that is **safe** by what the
  device knows: it carries every revocation of the account the device
  knows, its creator and every recipient are current, unrevoked devices of
  the account, and it is sealed to the current recovery key. Otherwise the
  device queues uploads and creates the next generation itself (a fresh
  key); if Connect already holds an unsafe manifest for that number, it
  takes the next free number. It never falls back to an unsafe or older
  generation, so a withheld manifest can stall uploads, never weaken them.
  Once a device learns of a revocation, every generation made before it is
  unsafe, even one a transfer the device never saw may have brought to the
  revoked device, so a new one is made before its next upload; if no
  remaining device is online, the next one to connect does it first. A
  revocation withheld by Connect is not learned, so until then uploads
  continue under the old generation.
- **Delivering keys later** (to a new device, or to a new recovery key) uses
  a **key transfer**, signed by a current device of the account: account
  key, manifest hash and generation, the recipient (device key and X25519
  key, or recovery key version), and the HPKE encapsulation and ciphertext.
  The sender first learns the manifest's revocations and never transfers to
  a device revoked by them or by its own feed. Manifests never change. An
  existing device sends transfers for every generation it can open when it
  sees a new device's certificate; until one is online, the new device reads
  nothing from the archive.
- **Changing the recovery code** (which needs the password and a signed-in
  device) publishes a new recovery key version; that device then sends
  transfers of every generation it can open to the new recovery key and
  creates a new generation sealed to it. If it cannot open some generation,
  it says so: "History before … can't be restored with the new recovery
  code." With that caveat, the recovery code opens the whole archive, so
  history survives losing all devices.
- Older generations stay usable for **reading**; only uploads follow the
  rules above.
- Each record holds its context (server key, group id, epoch) and key inside
  the encryption, under a random record id; the AAD binds account key,
  generation and record id. Imports only add keys and never overwrite.
- A new device that can open the archive gets your history, also in channels
  without history for new members.
- An older or partial archive can only hide keys, and so hide newer revisions
  or deletions on that device; it cannot reveal anything. It never restores
  MLS state, membership or the current generation.

Local identities have one device and no archive.

## Voice

### Keys

- A **call** is one voice group: it starts when the first device enters a
  channel's voice and ends when the last one leaves. Its descriptor names the
  channel's group, and its creator must be a member of that group; joiners
  enter by external commit and are checked against the channel's rules. The
  server proposes a device's removal on leave or disconnect, and any
  participant commits. The server maps each leaf to the session it joined
  from. Pins, keys, replay state and acknowledgements live only as long as
  the call; while a client is in a call, it refuses another group for the
  same channel.
- Each sender's key is
  `MLS-Exporter("gwar voice v1", leaf_index (u32, big-endian), 32)` in the
  current epoch, used as the SFrame base key. The SFrame key id is
  `epoch (low 32 bits) ‖ leaf_index (32 bits)`, so receivers find the key per
  frame even though the SFU moves speakers between the 8 receive slots.
  The key id carries only the low half of the epoch, so receivers keep keys
  and replay windows per full `(group, epoch, leaf)`; a frame whose key id
  matches several kept epochs is tried newest first (at most eight), and
  only a frame that authenticates changes any state.
- A leaf index is not a device. When a device is removed, its keys go from
  every kept epoch before its freed leaf can belong to another device, and
  acknowledgements are counted per device of the validated epoch.
- One encryptor owns each sender key and its SFrame counter. An engine that
  restarts, or cannot prove its counter continued, does not reuse the key: it
  stays silent and commits an update of its leaf, then sends in that new
  epoch once it is active. Until then it accepts no key from before that
  update, for sending or, when its replay windows were lost, for receiving.
  Others keeping its old key to receive is harmless.
- Every participant can derive every sender's key, so a participant could
  forge another's audio. Accepted for now (see Not goals).

### Epoch changes

- An epoch change is identified by `(group_id, epoch)`. A participant
  acknowledges it (`voice.epoch.ack`, relayed by the server to the call) only
  once the new keys are installed in its actual decryptor (worker or native
  engine). Acknowledgements for older epochs are ignored; those for an epoch
  the participant has not processed yet are kept (a bounded number) until it
  does. A participant that caught up after a gap asks the call to repeat
  acknowledgements of the newest epoch.
- Each sender **activates** the newest epoch it has processed by one rule:
  if any commit since its current sending epoch removed a leaf, at once;
  otherwise (additions, updates) when every participant of the new epoch,
  newcomers included, has acknowledged it, or two seconds after the sender
  processed it, whichever comes first. A newer commit replaces a pending one
  and keeps the earlier start of the two-second wait, and a removal anywhere
  in the pending chain keeps it immediate. "At once" means the sender stops
  using the old key the moment it processes the commit, even while the new
  keys are still being installed; it stays silent until they are.
- The two seconds count from when the sender processed the commit. They
  bound the wait for acknowledgements, not delivery or key installation: a
  commit the server withholds delays the change until it arrives (see
  "Freshness").
- A removed participant gets nothing under keys it does not have, whatever
  the SFU does; the SFU also stops forwarding to it. It can still decrypt a
  sender who has not processed its removal yet, until that sender does; that
  is the cutoff, per sender, and frames encrypted before it stay readable to
  it whenever they arrive. A newcomer hears others once they activate; the
  two-second limit bounds the wait, so a slow device can briefly miss audio.
- Receivers keep an epoch's keys while it is the newest epoch in which some
  current device's frame authenticated (silent senders included, through
  their last frame; a late frame from an older epoch never moves this back),
  or for ten seconds after the receiver processed the first commit that
  superseded it, whichever is longer; keys of removed senders go at once.
  At most eight epochs are kept per call; past that the oldest goes, and a
  sender that far behind stays silent to that receiver until it catches up
  and sends in a newer epoch.
- Acknowledgements are relayed by the server and not signed, so a dishonest
  server could make senders switch early (a newcomer misses a moment of
  audio) or hold them until the two-second limit. That only affects
  availability: keys always come from locally validated MLS state, and
  removals never wait.

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
  server, session, voice group and full epoch, with receive keys installed
  separately from sender activation, and ignored after the run ends.
  Repeating an identical install keeps the counter and replay windows;
  different keys for the same binding stop the engine. An install overtaken
  by a newer one is ignored, and no install brings back keys that were
  purged or replay windows that were reset.
- **Frame size**: Opus frames are kept small (20 ms at the current bitrates
  is about 120–200 bytes, plus at most 33 bytes of SFrame). Clients drop,
  never truncate, an encoded frame over 1000 bytes before encryption, so a
  frame always fits one RTP packet (str0m 0.24 leaves about 1120 bytes).

### SFU

- It forwards the encrypted payload unchanged, as opaque bytes: str0m runs in
  RTP mode, which gives the SFU each packet's SSRC, extended sequence number,
  timestamp and header extensions without depacketizing Opus (S4).
- **Streams**: each call and each engine run sends on a fresh SSRC, in
  plaintext channels too. Leaving a call (including a move to another
  channel) discards every frame captured, encoded or queued for it in the
  client; a frame is never sent under another call's SSRC. The server binds
  an SSRC to the call when it is negotiated, retires the departing stream
  at once, and drops packets of unknown or retired SSRCs before routing
  them (retired ones are bounded per microphone, refusing new ones past the
  limit). So a frame delayed from an earlier call or run never enters
  another one, and a sequence number only restarts with a new SSRC.
- **Timing**: today a slot's RTP time advances by the previous packet's
  duration, read from the Opus header, which is now encrypted. Instead:
  - senders stamp each packet with the capture position of its first sample
    (48 kHz), which also advances over frames not sent (silence, mute,
    push-to-talk, dropped frames). Browsers do this already; the native
    engine counts positions where audio is captured, carries them with each
    frame instead of using wall-clock time, and discards a partial frame
    across a gap in capture;
  - per sender stream (SSRC), the SFU orders packets by extended RTP
    sequence number: duplicates and packets older than the last forwarded
    are dropped. A new SSRC starts a new baseline;
  - for a newer packet from the same speaker on a slot, the forwarded time
    advances by the sender's timestamp difference (modulo 2^32) and the
    packet goes out at once. A difference that is negative or over five
    seconds despite a newer sequence number is a source reset (so is a
    real silence longer than five seconds, harmless when packets arrive on
    time); a
    difference of zero (one frame per packet, so a sender fault) drops the
    packet and is counted;
  - on a speaker change or a source reset, the forwarded time advances by
    the elapsed wall-clock time rounded up to whole 20 ms frames, at least
    one.
- **Frame duration**: clients ask for 20 ms frames (`ptime`/`maxptime` 20;
  the native engine always uses 20 ms). The sender checks each frame's Opus
  duration before encrypting and counts other durations; they are still
  sent, because timing rests on the sender's timestamps and only the
  speaker-change estimate assumes 20 ms. In S2 all three browsers kept 20 ms.
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
  encrypted voice, and no channel text crosses in either direction. Each
  audio packet and each queued text keeps the channel it came from, and
  whether that channel is encrypted, from the moment it enters the server;
  every later boundary (the SFU's puppet feed, the puppet socket, delivery
  queues, query and listener routes) checks it again, together with the
  generation and confirmed TeamSpeak channel of the puppet or connection
  that route uses, so a move never
  changes where a queued packet may go. All writes to TeamSpeak go through
  these checks. Plaintext audio or text queued across a move is dropped,
  never re-sent into the new channel.
- An encrypted channel's description on TeamSpeak is a fixed notice, never
  its participant list; nothing of an encrypted conversation (text,
  attachment names, titles) is copied into public metadata.
- Direct messages and server chat are classified by their own conversation,
  never by the voice channel the sender is in.
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
which addresses, and the number and size of records; archive generation
numbers, who created each manifest and for which devices, recovery key
versions and key transfers. Record contents, including which server, group
and epoch a key belongs to, are encrypted.

## Protocol changes (sketch)

`vc/4`, with no compatibility with `vc/3`:

- `mls.key_packages.publish`, `mls.key_packages.claim {uid}`;
- `mls.group_info {group}`, `mls.commit {group, epoch, commit, welcome?,
  group_info}` answered with ok or `stale`, event `mls.commit`;
- `mls.history.share {group, recipient, bundle}`, `mls.history.request`,
  event `mls.history`;
- `dm.open {uid}` returning the pair's group (creating it once);
- `owner` in `Welcome`, the owner chain and manager list (`owner.chain`,
  `managers.update`), pending managers;
- `chat.send`/`chat.edit` carrying `{epoch, ciphertext, file_ids}` in
  encrypted conversations, `chat.delete_own`; message statement v3 and body v3
  everywhere; `Channel` gains `encrypted`, `history`, `private`;
- `voice.epoch.ack`;
- Connect: `/v1/archive` (append, list since cursor), archive manifests
  (first one per generation wins), device X25519 keys, the recovery public
  key.

## Spikes

Throwaway code on a branch; only results come back into this document.
Measurements name the hardware and browser versions and report medians and
p95/p99.

| Spike | Question | Passes when |
| --- | --- | --- |
| S1 MLS in the web app | Does OpenMLS 0.9 work in WebAssembly for create, external commit, add/remove, external-sender removals, exporter, GroupContext extensions, in Chrome, Firefox and Safari? Can it sign with the existing device key? Can a client catch up through epochs holding expired leaves and then remove them? Bundle size, speed, persistence in IndexedDB? | All operations work in the three browsers; gzip size under 400 KB; a commit in a 100-device group under 100 ms (p95) on a laptop; state survives reloads and a crash mid-commit. Otherwise mls-rs or ts-mls is evaluated the same way. |
| S2 SFrame in browsers | Does `RTCRtpScriptTransform` encrypt to our SFU and decrypt across slot changes in Chrome, Firefox and Safari, failing closed? | Clean audio with eight senders; speakers moving between slots keep decrypting; added latency p99 under 2 ms including worker queueing, also with the tab in the background; no plaintext frame ever sent or played. |
| S3 SFrame native | Does `sframe` 2.0 in `vc-client` interoperate with S2? | Shared test vectors pass both ways; frames at the 1000-byte cap with the widest key id and counter, and the negotiated RTP extensions, cross the real SFU intact in both directions; larger frames are dropped. |
| S4 SFU and latency | Timestamp-based forwarding, speech gate, end-to-end cost. | Talking indicators and slots unchanged; mouth-to-ear p95 within 2 ms of plaintext on the VM; correct timing with packet loss, reordering, duplicates, timestamp wrap and resets, silence, mute, burst delivery and resumed speech. |
| S5 Churn | Joins, removals, restarts, overlapping transitions, lost, fabricated or duplicate acknowledgements, withheld commits, repeated key installation, leaf reuse, epoch wrap, silent senders across epochs, more than eight epochs in a row, a call ending and a new one starting with returning devices, stale `voice_set_keys` calls. | Zero nonce reuse; a removed participant decrypts nothing after the removal commit is processed; no plaintext fallback; audio resumes after every transition (time to usable audio measured). |
| S6 TeamSpeak isolation | With the official TS3 server: moves, whispers, puppets in their grace period, query listeners, restarts. | Zero bytes of an encrypted channel's audio or text reach TeamSpeak, and no TeamSpeak audio, whisper or text enters an encrypted conversation; plaintext channels keep working both ways. |

### Results

Measured in October 2026 on an Apple M5 laptop (macOS 27) with Chrome 155,
Firefox 155–157 and WebKit 26–27 through Playwright, and S1–S3 again in
Safari 27 through WebDriver. Times are median / p95 / p99.

- **S1: passed with OpenMLS except for size**, which the choice above
  accepts. Every operation worked in all three browsers, 20 runs each,
  including signing with the existing device key and catching up through five
  epochs with an expired certificate before removing it. A commit in a
  100-device group took 9.4 / 10.1 / 10.5 ms in Chrome and at most 12 ms p95
  in Firefox and WebKit; a whole local round with the IndexedDB write took
  19–30 ms median. State survived reloads and nine forced browser kills per
  browser between creating and merging a commit. In Safari 27 the
  operations, the expired-certificate catch-up and a reload passed too
  (commit 11 / 11 / 11 ms, local round 22 ms median); forced kills were not
  repeated there, so as not to close the owner's own Safari. Gzip size
  447 KB (mls-rs: 267 KB, every gate passed).
- **S2: the functional checks passed; the latency gate is only partly
  measured.** Eight senders decrypted correctly through the real SFU, also
  while moving between slots; injected plaintext, tampered, replayed and
  unknown-key frames silenced only their sender, and a sender without keys
  sent nothing. From receiving a frame to handing it on decrypted, p99: Chrome
  0.10 ms (also in a hidden tab), Firefox 0.56 ms (0.82 ms with the window
  covered); WebKit and Safari give no receive time, and decryption itself took
  0.10 ms in WebKit and 0.08 ms in Safari (0.66 ms with its window minimised).
  The Firefox and WebKit figures come from a rerun on an idle machine. Safari
  also passed the browser functional checks, as receiver and sender with
  Chrome, and exchanged frames with the native engine both ways (decrypted and
  decoded; desktop playback not tested). During parallel builds Firefox
  reached 62 ms p99 with a covered window, so behaviour under CPU load belongs
  to the latency work. Not measured: the queues before the sender transform
  and after the receiver's, which browsers do not expose, and hidden Firefox
  and WebKit tabs under normal background policy.
- **S3: interoperable with empty metadata, not fully RFC-conformant.** 42
  shared vectors both ways and all RFC header vectors; 200 frames of 1000
  bytes with the widest key id and counter crossed the SFU intact in each
  direction for each browser, Safari included; larger frames were dropped
  before encryption. `sframe` 2.0 fails the RFC vector with non-empty metadata
  (see "Building blocks").
- **S4: passed locally.** Forwarding by sender timestamps added 0.17 ms at
  p95 (400 markers per variant, native clients over loopback, alternating
  runs, without SFrame); talking indicators and slots were unchanged, and
  loss, reordering, duplicates, both wraps, resets, silence, mute and bursts
  matched expected traces. The rules it added are under "SFU". Through the
  VM over the internet (about 34 ms round trip) the median of each call was
  between 155 and 208 ms for both builds, a spread set when each call starts
  (playout buffering), so this path cannot resolve the 2 ms gate. The
  loopback figure stays the measured forwarding cost, and the spread goes to
  the latency work.
- **S5: passed in simulation.** Real OpenMLS groups and `sframe` keys, with
  a modelled network and SFU, ran every scenario of the S5 row 40 times on
  three network profiles: 4,955,406 encryptions without a repeated key and
  counter, no frame encrypted after its sender processed a removal
  decrypted by the removed participant (166 million attempts), no
  plaintext, and audio
  back after every transition whose commits arrived. After a join, everyone
  heard everyone in the new epoch within 22 ms on a LAN, and 193 / 278 /
  295 ms at 50 ms round trip with 1% loss (the simulation counts no CPU
  time); when acknowledgements are lost or updates overlap, the change waits
  up to the two-second limit while the old keys keep the audio going. A
  commit withheld for good leaves that participant unable to hear the others
  once they switch, as designed. The rules it sharpened (full-epoch
  keys, leaf reuse, removal while keys install, kept and repeated
  acknowledgements, retention timing) are under "Voice". Not tested: the
  real key-install command, browsers and audio devices.
- **S6: passed at code level.** By the owner's decision this run used fakes
  in place of the official server. 17 tests, each with a plaintext control
  that proves the path works, cover moves during speech, the grace period,
  refused moves, whispers, text both ways, listeners and reconnects. The run
  with the official server remains, and so do frames delayed from before
  the server classifies them: the per-call SSRC rule under "SFU" is meant to
  close that gap, and is tested with the implementation.

## Left for the implementation spec

The direction above is settled; these are pinned down before building:

- canonical encodings and hash inputs of every signed statement, the owner
  chain's genesis, sequence numbers when devices sign concurrently, and
  atomic checkpoint storage;
- restart validation, including member lists bound to the successor's
  `group_id`;
- the state machine of an access list whose signer expires or is revoked
  (kept for history, no longer current), across renewals, owner changes and
  concurrent lists;
- durable storage of the revocations a device learns from manifests and its
  own actions, independent of the feed cursor, rechecked before every upload
  and transfer; the withheld-generation case is a regression test;
- crash-safe ordering of a recovery code change (credentials, recovery key,
  transfers, new generation), manifest predecessors across skipped numbers,
  and domain-separated recovery key derivation and HPKE context;
- what a device keeps as evidence of past membership, and imported versus
  locally derived keys when they disagree;
- voice key installation that is idempotent and keeps counters and replay
  windows, with retirement after authentication;
- per-SSRC RTP state from the transport (today's `AudioPacket` drops it),
  native playback of frames other than 20 ms, and bridge queues that keep
  each packet's originating channel across moves;
- a write-ahead outbox for commits, reconciled with the server by commit
  hash after a crash, and an external commit that stays inactive until the
  server accepted it (S1 tested recovery only at chosen points);
- a review of the WebCrypto SFrame worker code, and of the OpenMLS crypto
  provider and storage integration.

Still to measure before building: S6 with the official TeamSpeak server.

## Open questions

- A manager added by an admin other than the owner waits for the owner's
  client. Is that acceptable on servers whose owner is rarely online, or
  should the owner be able to delegate that confirmation?
- History bundles grow with every epoch (about 40 bytes each). If channels
  reach many thousands of epochs, a key tree can replace the flat list.
- SFrame suite for voice: `AES_128_GCM_SHA256_128` (16-byte tag) by default;
  whether a shorter-tag suite is worth it for bandwidth is not measured yet.
