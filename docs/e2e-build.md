# End-to-end encryption: build plan and implementation spec

Status: **building** (phase 4). [e2e.md](e2e.md) is the design and stays the
reference for *what* and *why*. This document pins down *how*: the order of
work, crate and module layout, exact encodings, wire messages and storage. A
part is specified here before it is built; later parts are only outlined
until their turn.

## Order

Each step is one pull request (or a short stack), reviewed, with TeamSpeak
and the web client working after every merge.

1. **MLS core** (`crates/gwar-mls`): Gwar's rules on top of OpenMLS, for the
   web app (WebAssembly), the server and tests. No user-visible change.
2. **Protocol `vc/4` and the delivery service**: key packages, group infos,
   ordered commits, the handshake log, server removal proposals; message
   statement and body v3 everywhere.
3. **Encrypted direct messages**: stored, delivered to offline members,
   history for a participant's new devices.
4. **Encrypted channels and server-wide chat**: channel creation options,
   encrypted attachments, mentions and unread counts after decryption,
   TeamSpeak text isolation.
5. **Encrypted voice**: voice groups, SFrame in the browser worker and the
   native engine, epoch acknowledgements, per-call SSRCs, TeamSpeak audio
   isolation, S6 with the official TeamSpeak server in CI.
6. **Private channels**: owner chain, manager list, access lists.
7. **Key archive** on Gwar Connect.
8. **Pre-release reviews**: the WebCrypto SFrame worker; `openmls_rust_crypto`
   and our storage.

Parts 1–3 are specified below. Voice comes before private channels and the
archive because it is the product's core and needs only public channel
groups.

## Changes to the design

Pinned while specifying; [e2e.md](e2e.md) is updated in the same pull request.

- **History bundles are sealed to a device key, not the leaf key.** A leaf's
  encryption key changes with every update of that leaf, and OpenMLS deletes
  the old private keys, so a bundle sealed to the key a newcomer had when it
  joined could become unreadable before it arrives. Each device instead
  publishes one X25519 **device HPKE key**, signed by its device key (the
  same key the archive design already needs). The signature binds it to the
  device key, which is the leaf signature key the group validates, so the
  bundle still reaches only that member device.
- **Encrypted direct messages carry no attachments** in step 3, as today's
  direct messages carry none. Encrypted attachments arrive with channels in
  step 4, for direct messages as well.

## 1. MLS core: `crates/gwar-mls`

A Rust library with no WebAssembly dependencies, used natively by the server
and tests, and by `crates/gwar-mls-wasm`, a thin `wasm-bindgen` wrapper for
the web app.

### Building blocks

- `openmls` 0.9 (feature `js` only in the wasm wrapper), `openmls_rust_crypto`
  0.6, `openmls_traits` 0.6, pinned exactly.
- Ciphersuite `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`, only.
- Wire format policy `PURE_PLAINTEXT_WIRE_FORMAT_POLICY`: handshake messages
  are `PublicMessage`, so the server can order commits and see senders, as
  the design requires. Gwar sends no MLS application messages; chat uses
  epoch keys (below).
- Ratchet tree extension in every GroupInfo and Welcome.
- `RequiredCapabilities` in every group: extension `0xff00` (descriptor),
  credential type Basic. Every leaf and key package lists `0xff00`.

### Credentials

The `BasicCredential` identity is the UTF-8 text, `\n`-separated, no trailing
newline:

```text
gwar credential v1
{account_key}    the Connect account key, or - for a local identity
{device_key}
{issued_at}      certificate fields, or - for a local identity
{expires_at}
{certificate_signature}
```

All keys and signatures are canonical unpadded base64url. The uid is
`uid_for_key(account_key)`, or `uid_for_key(device_key)` without an account.
A credential is **well-formed** when it parses canonically (re-encoding gives
the same bytes), the leaf's signature key equals `device_key`, and for an
account: the certificate verifies as `gwar device v1` (vc-proto's
`device_statement`), `expires_at - issued_at` is at most 90 days.

Whether it is **current** is judged separately, against a time and a
revocation view supplied by the caller (`Authority` below): not expired, and
not revoked at or before that time.

### Descriptor

The GroupContext extension `0xff00` holds three netstrings (as in
`canonical_message_body`): the **statement**, the creator's **credential**
identity, and the creator device's Ed25519 **signature** of the statement
(raw, base64url).

```text
gwar conversation v1
{server_key}
{conversation}          dm:{uid}:{uid} | channel:{id} | server | voice:{channel id}
{kind}                  dm | channel | server | voice
{incarnation}           decimal, 0 first
{previous_group_id}     base64url, or -
{private}               0 | 1
{history}               0 | 1
{founding_owner}        uid, or -
{channel_group_id}      base64url, or -
{creator_uid}
{created_at}            Unix ms
{creation_nonce}        16 random bytes, base64url
```

`group_id = SHA-256(statement)` (raw 32 bytes). Parsing checks every field
strictly (canonical numbers and base64url, `kind` matches `conversation`,
`previous_group_id` is `-` exactly when `incarnation` is 0, `channel_group_id`
is set exactly for voice, `founding_owner` only for private channels; for a
direct message `private` is 0 and `history` is 1, and both uids are distinct
and sorted), the creator credential is well-formed with uid `creator_uid`, and
the signature verifies with its `device_key`.

### External sender

Every group's `external_senders` holds exactly one entry: the server's
Ed25519 key, with the Basic credential `gwar server v1\n{server_key}`. Clients
compare it with the server key they pinned for the endpoint.

### What the server uses (`gwar_mls::ds`)

The server follows every group as an OpenMLS **`PublicGroup`**: an observer
that holds the public ratchet tree and GroupContext, verifies every
`PublicMessage` handshake (signatures, tree, proposals) and merges commits,
without any secret. Its state per group is the observer's storage, saved as
one blob. `ds` wraps this with Gwar's rules:

- `key_package(bytes) -> KeyPackageInfo`: verifies the key package (signature,
  lifetime, suite, capabilities include `0xff00`), parses its credential
  (well-formed), returns device key, uid, `KeyPackageRef`, `not_after`, and
  whether it is a last-resort package.
- `Observer::create(group_info) -> Observer`: from a GroupInfo with the ratchet
  tree extension (`PublicGroup::from_external`, which verifies the tree and
  the GroupInfo signature); checks the descriptor extension parses
  (`SignedDescriptor::parse`), `group_id` is its hash, the external senders
  hold exactly the server key, and every leaf credential is well-formed.
- `Observer::commit(commit, group_info) -> Accepted`: processes a commit at the
  observer's epoch, checks Gwar's structural rules on the resulting tree
  (well-formed credentials, one leaf per device key, an update keeps uid and
  device key, the descriptor and external senders unchanged, the membership
  rule of the kind), merges it, and checks that the uploaded GroupInfo has
  the new epoch, the observer's new GroupContext and a valid signature.
  Returns the sender (`Member(index)` or `NewMember`), the leaves added and
  removed (index, device key, uid), and Welcome recipients are checked
  against the added leaves' `KeyPackageRef`s.
- `Observer::propose_remove(server_signer, leaf) -> bytes`: an external
  `Remove` proposal signed by the server key, also stored in the observer so
  that a commit may include it by reference. Commits may reference only the
  server's own proposals of that epoch; all other proposals are inline.
- `Observer::leaves()`: index, device key and uid of every member.

Certificate currency (expiry, revocation) is the clients' job; the server
checks only that credentials are well-formed, plus the rules under "Who may do
what".

### The client agent (`gwar_mls::Agent`)

One per device, holding the device's Ed25519 seed (the JWK `d` value) and
credential, the OpenMLS provider and Gwar's own per-group records.

- **Authority**: before processing, the caller supplies the time and the
  device's revocation view (account key, device key, revoked at) from its
  Connect feed. Leaf validation (design: "Identities in MLS") runs on every
  Welcome, external join, and staged commit before merge, with these rules:
  well-formed credential; current at the commit's processing time; one leaf
  per device key; an update keeps uid and device key; the membership rule of
  the descriptor's kind (direct message: only devices of its two uids;
  channel and server: any well-formed device, the server's word); the
  descriptor unchanged; the external senders unchanged and equal to the
  pinned server key. External proposals other than `Remove` are rejected.
- **Catching up vs. sending**: processing judges each commit by the rules as
  they stood at that epoch (credential validity at the commit's time). After
  merging, the agent reports leaves that are *not current now*; while any
  exist the group is **blocked**: no sending, no history sharing, and the
  caller commits their removal.
- **Freshness**: per group the highest epoch seen and its epoch
  authenticator; a GroupInfo or tree behind it, or another authenticator for
  the same epoch, is rejected.
- **Pins**: per `(conversation, incarnation)` the group id first joined.
- **Epoch keys**: after every merge and join,
  `epoch_key = MLS-Exporter("gwar chat v1", group_id, 32)` is stored with
  `(group_id, epoch)`, together with the uids that had leaves in that epoch
  (evidence for returning members, step 3). Keys stay until the conversation
  is deleted.
- **Commits** never merge locally before the server accepted them (write-ahead
  outbox, below). An external join creates no usable group until accepted.
- **Key packages**: `key_packages(n)` and `last_resort_key_package()`, with a
  lifetime ending no later than the device certificate.

### Messages (`gwar_mls::chat`)

- `message_key = HKDF-SHA256(salt = empty, ikm = epoch_key, info = "gwar
  message key v1" ‖ message_id (16 raw bytes) ‖ revision (u32 big-endian),
  L = 32)`.
- AES-256-GCM, a random 96-bit nonce; `ciphertext = nonce ‖ sealed ‖ tag`.
- AAD (UTF-8): `gwar chat ciphertext v1\n{server_key}\n{conversation}\n{group_id}\n{epoch}\n{message_id}\n{revision}`.
- The plaintext is JSON `{"text", "mentions", "attachments", "signature"}`
  with a v3 signature (step 2). Opening checks that `conversation`,
  `group_id`, `epoch`, `message_id` and `revision` inside the signature equal
  those outside; the caller then verifies the signature as for any message.

### History bundles (`gwar_mls::history`)

- **Device HPKE key**: X25519, generated once per device, kept with the MLS
  state. Published with the statement
  `gwar device hpke v1\n{device_key}\n{hpke_public_key}\n{created_at}`,
  signed by the device key.
- A bundle is HPKE base mode (`DHKEM(X25519)`, HKDF-SHA256, AES-128-GCM, as the
  MLS suite) to the recipient's device HPKE key, `info = "gwar history v1" ‖
  group_id`. The plaintext lists `(epoch u64 BE, key 32 bytes)` entries for
  one group, sorted by epoch, unique.
- The sender's device key signs
  `gwar history bundle v1\n{server_key}\n{group_id}\n{recipient_device_key}\n{recipient_hpke_key}\n{sender_device_key}\n{created_at}\n{sha256(enc ‖ ciphertext)}`.
- The recipient accepts it only from a sender whose device key is a current
  member of the group in the recipient's current epoch, imports only epochs it
  lacks (a key it derived itself always wins), and never overwrites.
- Which epochs a sender includes: for a newcomer in a group whose descriptor
  says `history = 1`, every epoch key it holds; for a returning member, only
  epochs whose evidence lists that uid; in a group with `history = 0`, only
  the latter.

### Web app storage and concurrency

- MLS runs in the page's main thread, in the wasm module loaded on first use
  (gzip budget 450 KB, checked in CI).
- Only one tab per device runs MLS: the Web Locks API lock
  `gwar-mls:{device_key}` is taken before connecting and released when the
  connection is replaced or closed.
- IndexedDB database `gwar-mls-v1`: `openmls` (the provider's key/value
  pairs), `groups` (Gwar records per group: conversation, incarnation, pin,
  freshness, blocked state), `epochs` (epoch keys and evidence), `outbox`.
- Every agent operation ends with `take_changes()`: the changed and deleted
  keys of all stores, written by the caller in **one** IndexedDB transaction
  before anything is sent. A crash before the write loses the operation; a
  crash after it finds the outbox.
- **Outbox**: before sending a commit, its record (group, epoch, commit hash,
  commit, Welcome, GroupInfo) is written with the pending commit. After a
  reconnect, the client reads the handshake log from that epoch: if the
  commit there has the same hash it merges, otherwise it discards the
  pending commit, processes the log and retries if still needed.

## 2. Protocol `vc/4` and the delivery service

`PROTOCOL_VERSION = 4`; the hello statement becomes `gwar client hello v4`.
No compatibility with `vc/3`.

### Message statement and body v3

Everywhere, plaintext conversations included:

```text
gwar message v3
{server_key}
{conversation}
{group_id}       base64url, or - in a plaintext conversation
{epoch}          decimal, or - in a plaintext conversation
{author_uid}
{key}
{certificate_hash}
{message_id}
{revision}
{previous_revision_hash}
{created_at}
{signed_at}
{body_hash}
```

Body v3 adds three netstrings per attachment after today's four: the file
key, the ciphertext's SHA-256 and the ciphertext size, each `-` for a
plaintext attachment. `MessageSignature` gains `group_id` and `epoch`
(strings). The server keeps verifying plaintext conversations as today, with
v3.

### Delivery service requests

All binary fields are base64url. Sizes are capped (a key package 4 KiB, a
GroupInfo or commit 256 KiB, a Welcome 256 KiB, a history bundle 64 KiB).

| Request | Reply | Rule |
| --- | --- | --- |
| `mls.device.publish {hpke_key, created_at, signature}` | `{}` | the session's device; replaces an older one |
| `mls.key_packages.publish {packages, last_resort?}` | `{count}` | each parsed with `ds::key_package`, device and uid of the session; at most 20 stored plus one last-resort |
| `mls.key_packages.claim {uids}` | `{packages: [{device_key, package}]}` | one per device of those uids with a package, except the caller's device; consumes a regular one, else returns the last-resort one; skips revoked devices |
| `mls.devices {uids}` | `{devices: [{uid, device_key, hpke_key, created_at, signature}]}` | published device HPKE keys |
| `mls.group.create {conversation, group_info}` | `{group}` | the GroupInfo of epoch 0, holding only the creator's device; `Observer::create` accepts it; the session may create it (below); first wins per `(conversation, incarnation)`, later attempts get `conflict` with the existing group id |
| `mls.group_info {group}` | `{group_info, epoch}` | the session may read the conversation |
| `mls.commit {group, epoch, commit, group_info, welcome?}` | `{epoch}` or error `stale` | below |
| `mls.log {group, since, limit}` | `{entries: [{epoch, commit}]}` | commits from epoch `since`, 90 days kept |
| `mls.groups {}` | `{groups: [{conversation, group, epoch}]}` | every group the session may read |
| `mls.history.share {group, recipient, bundle}` | `{}` | stored per recipient device, at most three sender uids per (group, recipient) |
| `mls.history.fetch {group}` | `{bundles}` | bundles for the session's device |
| `mls.welcomes {}` | `{welcomes: [{group, welcome}]}` | Welcomes for the session's device; each deleted once fetched |

Events: `mls.commit {group, epoch, commit}`, `mls.proposal {group, epoch,
proposal}`, `mls.welcome {group}`, `mls.history {group}`; the last two only
announce that `mls.welcomes` or `mls.history.fetch` has something.

### Commit ordering

The core actor handles `mls.commit` atomically. The commit must be for
`group` at exactly the stored epoch, else `stale`; then
`Observer::commit` must accept it, else `bad_request`. An external join
(`NewMember`) is allowed only for a session whose uid may read the
conversation, and its new leaf must be that session's device. The commit, the
new GroupInfo, the observer state, the leaf map and the Welcome are stored in
one transaction; then `mls.commit` goes to every session that may read the
conversation, and the Welcome to its recipients.

### Removal proposals

The server proposes `Remove` (signed by its key, via `Observer::propose_remove`)
for every leaf of a device that: was revoked (Connect feed), belongs to a
member who was removed from the server or banned, or (direct messages) does
not belong to a participant. Proposals are re-issued for the new epoch after
each commit while the leaf is still there, and sent as `mls.proposal` to
sessions that may read the conversation. Any member commits them; the
server only checks that a commit is well-formed and in order.

### Who may do what

- Direct message `dm:{a}:{b}`: sessions of uids `a` and `b`, both members of
  the server. The creator is one of them.
- Channel and server groups (step 4): sessions that can read the channel, or
  every member.

### Storage (server)

New tables (migration in `store.rs`): `mls_devices(device_key, uid,
hpke_key, created_at, signature)`, `mls_key_packages(ref, device_key, uid,
package, last_resort, not_after)`, `mls_groups(group_id, conversation,
incarnation, epoch, group_info, observer, created_at)`, `mls_log(group_id, epoch,
commit, created_at)`, `mls_leaves(group_id, leaf, device_key, uid)`,
`mls_welcomes(device_key, group_id, welcome, created_at)`,
`mls_history(group_id, recipient_device, sender_uid, bundle, created_at)`.
Expired key packages, log entries older than 90 days and Welcomes older than
90 days are pruned with the existing periodic cleanup. Deleting a
conversation (channel, member removal of a direct-message participant)
deletes its groups.

## 3. Encrypted direct messages

### Conversation and group

- Direct messages between two Gwar members are always end-to-end encrypted.
  Direct messages with TeamSpeak users stay plaintext, live-only and labeled
  "Not end-to-end encrypted", as today.
- Opening a conversation: `mls.groups` already lists it, or the client creates
  it: a descriptor (kind `dm`, incarnation 0, history 1), a group with its own
  leaf, registered with `mls.group.create`; then
  `mls.key_packages.claim {uids: [peer, self]}` and one `mls.commit` adding
  every claimed device, with its Welcome. On `conflict` it discards its group
  and joins the existing one.
- Devices without a key package (never online since the update), and new
  devices, join on connect by external commit, in the background (design:
  "Joining").
- History: when a member device sees a new leaf in a direct-message group, it
  sends a bundle (above) to that device after its next commit is accepted,
  unless the recipient already has bundles from three uids. A device that
  cannot open a message says "Earlier messages will appear when another
  member is online."

### Messages

| Request | Reply | Rule |
| --- | --- | --- |
| `chat.sealed.send {conversation, message_id, epoch, ciphertext}` | `SealedMessage` | the session may write the conversation; `message_id` unique in it; ciphertext at most 24 KiB |
| `chat.sealed.edit {conversation, message_id, revision, epoch, ciphertext}` | `SealedMessage` | only the author's uid; `revision` above the stored one |
| `chat.delete_own {conversation, message_id, revision, epoch, ciphertext}` | `SealedMessage` | the signed deletion revision; stored with `deleted`, no further edits |
| `chat.sealed.history {conversation, before?, limit?}` | `{messages}` | as channel history |
| `chat.read {conversation, message}` | `{}` | read marks per conversation |

`SealedMessage {id, conversation, author_uid, author_name, message_id,
revision, epoch, ciphertext, sent_at, edited_at?, deleted}`; event
`chat.sealed` for new messages, edits and deletions. The server stores the
latest revision only (as today), keeps at most the newest 1000 per
conversation, and counts unread messages per conversation for each
participant (no mentions: they are inside the ciphertext).

### Client

- The epoch is the sender's current epoch; a blocked group sends nothing and
  says "Securing this conversation…".
- Receiving: find the epoch key `(group, epoch)`, open, check the inner
  fields, verify the signature with today's `MessageVerifier` (revisions,
  revocations), then show it. Without the key: "Can't open this message on
  this device", muted, with the reason in the tooltip.
- A deletion the client cannot open shows the server's muted "deleted" line,
  never as a verified author deletion.
- Notifications for direct messages are computed after opening; a key
  arriving later never notifies twice.

### Tests

- `gwar-mls`: encodings (round trip, canonical rejection), descriptor and
  credential checks, every leaf rule with a regression per rule, freshness,
  outbox reconciliation, history bundle accept and reject paths, message
  seal/open with each field tampered.
- Server: commit ordering under concurrent commits, `stale`, external joins by
  the wrong uid, removal proposals after revocation and member removal, size
  limits, pruning.
- Web: two simulated devices per user against a real server in Playwright: a
  message to an offline user arrives after they connect; a second device sees
  history; a revoked device is removed and cannot read new messages.
