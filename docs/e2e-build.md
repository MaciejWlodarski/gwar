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

Parts 1–4 are specified below. Voice comes before private channels and the
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
- **Accepting a commit and sending are judged separately.** A client accepts
  a commit by structural rules and by certificates covering the commit's
  time for the leaves it introduces or changes, the same rules the server
  checks; revocations and later expiry never make it reject a commit, so
  clients with different Connect feeds follow the same chain. They block
  sending instead, until the leaves are removed ("The client agent").
- **Freshness uses the GroupContext, not the epoch authenticator.** The
  authenticator comes from epoch secrets and is not in a GroupInfo, so a
  client could not compare it for a GroupInfo it did not process. Clients
  checkpoint the SHA-256 of each epoch's GroupContext, which is public and
  commits to the tree and the transcript.
- **Encrypted direct messages carry no attachments** in step 3, as today's
  direct messages carry none. Encrypted attachments arrive with channels in
  step 4, for direct messages as well.

## 1. MLS core: `crates/gwar-mls`

A Rust library with no WebAssembly dependencies, used natively by the server
and tests, and by `crates/gwar-mls-wasm`, a thin `wasm-bindgen` wrapper for
the web app.

### Building blocks

- `openmls` 0.9.1, `openmls_rust_crypto` 0.6.0, `openmls_traits` 0.6.0,
  pinned exactly. The wasm wrapper enables `openmls`' `js` feature and
  `getrandom` 0.2's `js` feature (`openmls_rust_crypto` reaches `getrandom`
  0.2 through `rand_core` 0.6); CI builds the wrapper with production
  features.
- **Lifetimes.** OpenMLS checks key package lifetimes against the wall clock
  (at most 84 days, its limit). Gwar's rule is the device certificate, so:
  key packages last until the device certificate expires, at most 84 days;
  joins (Welcome, external commit) skip OpenMLS' lifetime check and apply
  Gwar's certificate rules instead; the server never re-validates lifetimes
  of leaves already in a tree (below). A device that cannot replay an old
  commit because an Add in it carries a key package expired since (only
  possible when catching up weeks late) rejoins by external commit and gets
  the epochs in between through history bundles. If it has an outbox at that
  epoch, a validated GroupInfo strictly newer than the source epoch may
  replace the pending state for this recovery; an equal/older head may not.
  Failure to validate exposes no changes and requires the usual reload.
- Ciphersuite `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`, only.
- Wire format policy `PURE_PLAINTEXT_WIRE_FORMAT_POLICY`: handshake messages
  are `PublicMessage`, so the server can order commits and see senders, as
  the design requires. Gwar sends no MLS application messages; chat uses
  epoch keys (below).
- Ratchet tree extension in every GroupInfo and Welcome, except GroupInfos
  of channel and server groups after creation (section 4, "Large groups").
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
`device_statement`), both times are safe integers and
`0 < expires_at - issued_at <= 90 days`.

Whether it is **current** at a time `t` is judged separately, against a
revocation view supplied by the caller (`Authority` below):
`issued_at <= t < expires_at`, and not revoked at or before `t`.

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

The shared context rules accept any incarnation; the server checks a
restart's predecessor at creation, and clients accept one only along its
chain (section 4, "Restarts").

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
- `Observer::commit(self, commit, group_info, welcome) -> (Observer,
  Accepted)`: consumes the observer, which the server loads from its stored
  state for each commit, so a rejected commit leaves nothing behind.
  Processes a commit at the observer's epoch, checks the commit time (see
  "Commit time") and Gwar's structural rules on the resulting tree
  (the client's "Acceptance" rules below, with no revocation view: only
  add, remove and external-init proposals; leaves the commit introduces or
  changes certified at its time; an external commit replaces only the
  joiner's own earlier leaf), merges it, and checks the
  uploaded GroupInfo: the observer's new GroupContext and confirmation tag, a
  ratchet tree extension equal to the observer's tree (none in channel and
  server groups, section 4), and a signature by one of the members. Returns the sender (`Member(index)` or `NewMember`), the
  leaves added and removed (index, device key, uid), and the device keys the
  Welcome is for, checked against the added leaves' `KeyPackageRef`s.
- `Observer::propose_remove(server_signer, leaf) -> bytes`: an external
  `Remove` proposal signed by the server key, also stored in the observer so
  that a commit may include it by reference. Commits may reference only the
  server's own proposals of that epoch; all other proposals are inline.
- `Observer::leaves()`: index, device key and uid of every member.

Certificate currency now (expiry, revocation) is the clients' job; the server
checks what clients accept a commit by ("Acceptance" below: well-formed
credentials, and certificates covering the commit's time for the leaves it
introduces or changes), plus the rules under "Who may do what".

### The client agent (`gwar_mls::Agent`)

One per device, holding the device's Ed25519 seed (the JWK `d` value) and
credential, the OpenMLS provider and Gwar's own per-group records.

- **Acceptance**: every Welcome, external join and staged commit (before
  merge) must satisfy the **structural rules**, the same ones the server
  checks: well-formed credentials; one leaf per device key; an update keeps
  uid and device key; an external commit removes at most one leaf, and only
  the joining device's own earlier leaf (same device key and uid); the
  membership rule of the descriptor's kind (direct message: only devices of
  its two uids; channel and server: any well-formed device, the server's
  word); the descriptor, required capabilities and external senders
  unchanged, the latter equal to the pinned server key; external proposals
  only `Remove`. In addition, every leaf a commit **introduces or changes**
  (adds, the committer's path leaf, an external joiner) must have a
  certificate covering the commit's time (see "Commit time"). This applies
  to commits the device processes as a member; a Welcome or a GroupInfo it
  joins from carries no commit time and is judged by the structural rules
  alone (a newcomer cannot check the group's history either way; the server
  checked the admitting commit), followed by the usual "not current now"
  check below. That is all:
  revocations and certificates that ran out later never make a client reject
  a commit, because clients with different revocation feeds must still
  follow the same chain (design: "Removal").
- **Catching up vs. sending**: after every merge or join the agent reports
  the leaves that are **not current now** (at the caller's time, with its
  revocation view); while any exist the group is **blocked**: no sending, no
  history sharing, and the caller commits their removal, recomputing what is
  left if another commit wins first.
- **Sending gate**: whether the group is blocked is computed again before
  every send and history share, at the current time and with the current
  revocation view, not only after merges. A group with an outstanding server
  proposal (`mls.proposal`, `pending`) is blocked too, until a commit
  includes it; if the proposal removes the device's own leaf, the device
  rejoins instead (see section 3, "Messages").
- **Freshness**: per group the highest epoch seen and the SHA-256 of its
  GroupContext (public; it commits to the tree and the transcript). A
  GroupInfo behind that epoch, or with another GroupContext for the same
  epoch (a fork), is rejected.
- **Pins**: per `(server_key, conversation, incarnation)` the group id first
  joined; later incarnations only as successors (section 4, "Restarts").
  `create` persists a tentative **Creating** group: the gate reports
  `creating`, without a sending key, commits or history sharing.
  `settle(server, id, winner)` takes the group the server holds for the
  conversation (the `mls.group.create` reply `{group}`, the group its
  `conflict` names, or, after a restart or a lost reply, the one `mls.groups`
  lists, possibly none): if it is this group, the group becomes **Active**
  and epoch zero is derived; otherwise the creation's OpenMLS group and
  record (pin) are deleted, so the device can join the winner by Welcome or
  external commit, or create the conversation again. Settling checks no
  certificate, so it works after the creator's leaf expired. Persist before
  using or leaving the group.
- **Removal of this device**: a commit that removes the device's own leaf is
  processed as any other, but ends its membership: no epoch key is exported
  for the new epoch (OpenMLS refuses, and the device must not have it), the
  freshness checkpoint stays at the last epoch the device was a member of,
  all OpenMLS records for that group (including private epoch key pairs)
  are deleted before advancing the epoch, and the group is marked **removed**,
  keeping its application epoch keys and pin. The same complete OpenMLS
  cleanup applies to Rejoin/pruned recovery, losing or expired external joins,
  and a lost creation (which also drops the tentative pin). If the client may
  still read the conversation, it rejoins by external commit from the current
  GroupInfo, whose epoch is later than the checkpoint.
- **Epoch keys**: after every merge and join that leaves the device a member,
  `epoch_key = MLS-Exporter("gwar chat v1", group_id, 32)` is stored with
  `(group_id, epoch)`, and the group's evidence records which uids had leaves
  in that epoch, as epoch intervals per uid (for history sharing in groups
  without history, below). Keys stay until the conversation is deleted.
- **Certificate renewal**: loading an agent with a renewed credential makes
  each subsequent member commit publish it in the committer's path leaf. Do
  this before the old leaf expires; an expired own leaf requires rejoining.
- **Commits** never merge locally before the server accepted them (write-ahead
  outbox, below). An external join is different: OpenMLS merges it when it is
  built, so the agent keeps that group as **joining**, persisted but not
  used (no sending, no epoch keys exported for use) until the server accepts
  the commit; if another commit wins, the joining group is deleted.
- **Key packages**: `key_packages(n)` and `last_resort_key_package()`, with a
  lifetime ending no later than the device certificate.

### Messages (`gwar_mls::chat`)

- `message_key = HKDF-SHA256(salt = empty, ikm = epoch_key, info = "gwar
  message key v1" ‖ message_id (16 raw bytes) ‖ revision (u32 big-endian),
  L = 32)`.
- AES-256-GCM, a random 96-bit nonce; `ciphertext = nonce ‖ sealed ‖ tag`.
- AAD (UTF-8): `gwar chat ciphertext v1\n{server_key}\n{conversation}\n{group_id}\n{epoch}\n{message_id}\n{revision}`.
- The plaintext is JSON `{"text", "mentions", "attachments", "signature"}`
  with a v3 signature (step 2). `chat::seal` takes the plaintext JSON bytes
  and encrypts them unchanged; `chat::open` returns the decrypted bytes.
  Both parse only `server_key`, `conversation`, `group_id`, `epoch`,
  `message_id` and `revision` from `signature` and require them to equal the
  outer context. The web caller parses and validates the full plaintext,
  checks the certificate, body hash and v3 author signature before returning
  it; the UI additionally applies revision continuity and revocation rules.
  Opening uses a read-only agent query, with no persistence transaction.
  Missing keys return `no_key`, invalid envelopes/authentication return
  `cannot_open`, and operational failures propagate to the caller.

### History bundles (`gwar_mls::history`)

- **Device HPKE key**: X25519, generated once per device, kept with the MLS
  state. Published with the statement
  `gwar device hpke v1\n{device_key}\n{hpke_public_key}\n{created_at}`,
  signed by the device key. Its creation time is a positive JavaScript safe
  integer. Loading checks this bound and derives the public key from the stored
  private key to reject inconsistent pairs; wasm returns the time as a number.
- A bundle is sent as **chunks** of at most 1000 epochs each; a chunk's
  plaintext lists `(epoch u64 BE, key 32 bytes)` entries for one group,
  sorted by epoch, unique, all within its range.
- Each chunk is sealed with HPKE base mode (`DHKEM(X25519)`, HKDF-SHA256,
  AES-128-GCM, as the MLS suite) to the recipient's device HPKE key, with
  `info = "gwar history v1" ‖ group_id` (raw 32 bytes) and `aad` the UTF-8
  header
  `gwar history chunk v1\n{server_key}\n{group_id}\n{recipient_device_key}\n{recipient_hpke_key}\n{sender_device_key}\n{first_epoch}\n{last_epoch}\n{created_at}`
  (keys and group id base64url, numbers decimal).
- The sender's device key signs `{header}\n{sha256(enc ‖ ciphertext)
  base64url}`.
- The `chunk` on the wire is netstrings (as in `encoding`) of: the header,
  `enc` (32 bytes), the ciphertext, the signature (64 bytes). The recipient
  checks that the header names itself, its current HPKE key, the group and
  the server, then verifies the signature, then opens. `gwar-mls` tests
  include a fixed seal/open vector.
- The recipient accepts it only from a sender whose device key is a current
  member of the group in the recipient's current epoch. Each stored key
  records its provenance: **derived** (by the device itself) or **imported**
  (with the sender uid). A derived key always wins and imports for that epoch
  are ignored. If two imports for an epoch disagree, the epoch is marked
  **conflicted**: its imported key is deleted and later imports for it are
  ignored, whatever the order they arrive in.
- Which epochs a sender includes: in a group whose descriptor says
  `history = 1`, every epoch key it holds (derived or imported), to any
  member device; in a group with `history = 0`, only epochs it derived whose
  evidence lists the recipient's uid. So there is no need to tell newcomers
  from returning members: with history on, a returning member may see what
  any newcomer sees; with history off, evidence decides, and imported epochs
  (which carry no evidence) are never passed on.

### Web app storage and concurrency

- MLS runs in the page's main thread, in the wasm module loaded on first use
  (gzip budget 480 KB, checked in CI).
- Only one tab per device runs MLS: the Web Locks API lock
  `gwar-mls:{device_key}` is taken before connecting. Before releasing it
  (connection replaced or closed) the tab finishes the operation in progress,
  including its write, and starts no other.
- One IndexedDB database per device, `gwar-mls-v2-{device_key}`, with stores
  `openmls` (the provider's key/value pairs), `groups` (Gwar records per
  group, keyed by server key and group id: conversation, incarnation, pin,
  freshness, evidence), `epochs` (epoch keys with provenance, conflicts),
  `outbox`. The browser adapter reserves UTF-8 `@runtime/` keys in that
  outbox for package publication and history jobs; it filters those keys
  before passing the four-store snapshot to the core. Core commit keys keep
  their server/group form and strict validation. History intents for newly
  admitted devices are written in the same transaction as the accepted
  group transition; sealed chunk bytes are persisted before sending and
  reused on retry for server deduplication. Each sealed history job also stores
  the recipient's signed device HPKE publication. Before retries the runtime
  fetches publications again; a strictly newer publication is verified by the
  core and resealed chunks replace the old job in one durable write before
  sending. Older or unchanged publications do not regenerate chunks.
- Loading validates canonical group keys and requires epoch and outbox keys
  to name a known group exactly. Epoch suffixes are exactly 20 decimal digits
  encoding a u64; history chunk spans use checked arithmetic.
- Agent operations run one at a time. Mutations end with `take_changes()`: the
  changed and deleted keys of all stores, written by the caller in **one**
  IndexedDB transaction before anything is sent and before the next
  operation starts. If the operation fails (an OpenMLS error or a Gwar rule;
  OpenMLS may already have changed state, such as deleting the key package a
  rejected Welcome named) or its write fails, nothing is written: the
  in-memory agent is dropped and loaded again from IndexedDB. A crash before the write loses the operation;
  a crash after it finds the outbox.
- **Outbox**: before sending a commit, its record (group, epoch, commit hash,
  commit, Welcome, GroupInfo) is written with the pending commit. After a
  reconnect, the client reads the handshake log at that epoch:
  - the same commit hash there: merge the pending commit;
  - another commit there: discard the pending commit, process the log, and
    commit again if still needed;
  - no entry at that epoch yet (the log's head is still that epoch): nothing
    won, so send the same bytes again. Only when the server answers
    `expired` (the commit's time is more than five minutes behind its commit
    clock, which never goes back, so no earlier copy can be accepted any
    more) is the pending commit discarded and a new one built (fresh time,
    current rules). On any other error (`early`, `bad_request`, …) it is
    kept and sent again with backoff: some checks depend on time (key
    package lifetimes against the server's system clock), and a commit
    becomes `expired` once the server's commit clock passes
    `committed_at + 5 min`, so a commit that is really broken holds the group
    up only until then (about five minutes when the clocks agree; longer if
    the commit's time was ahead or the server's clock was set back);
  - the log no longer reaches that epoch (its oldest retained epoch is
    later): drop the group state and rejoin by external commit.
  A joining group (external join) has no older state to fall back to: if
  another commit wins, or the server answers `expired`, it is deleted and the
  join is built again from the newest GroupInfo.

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
v3. On upgrade, the store migration sets `messages.signature` to SQL `NULL`
for every pre-v3 signature (missing `group_id` or `epoch`). Text, mentions,
attachments and timestamps remain intact and clients display this history as
unsigned Gwar messages. There is no v2 verification path. Author edits still
require a signed predecessor and return `bad_request` for unsigned history;
moderators with `message_manage` can remove it normally.

### Commit time

Every commit carries its time in the `PublicMessage` authenticated data,
signed with the commit by its sender: `gwar commit v1\n{committed_at}` (Unix
ms). The server rejects a commit whose time is more than five minutes behind
its commit clock with `expired`, one more than five minutes ahead of it with
`early` (it may be accepted later), and one earlier than the previous
commit's time of that group with `bad_request`.
The commit clock is the larger of the system clock and the highest value it
returned before, which is persisted (table `meta`) before any `expired`
reply, so a system clock set back never revives an expired commit. The
time of epoch 0 is the descriptor's `created_at`. Clients use a commit's time
only to check the certificates of the leaves it introduces or changes (see
"Acceptance"); they never compare it with their own clock, so devices with
different clocks follow the same chain.

### Size limits

Decoded sizes: a key package 4 KiB; a commit, GroupInfo or Welcome 128 KiB
each; a history bundle chunk 48 KiB. The gateway's limit for one WebSocket
message from an authenticated session rises to 640 KiB: the largest request,
`mls.commit` with three 128 KiB blobs, is about 512 KiB in base64, and the rest
is headroom for JSON. Before a successful `join`, frames and assembled messages
are capped at 96 KiB at the transport layer. A header guard checks declared
frame lengths and cumulative fragmented-message lengths before passing the
header to tungstenite, so an oversized frame is rejected without reading its
body or reserving its declared payload. After join the guard is disabled and
tungstenite enforces 640 KiB. Direct messages stay far below these limits; groups whose GroupInfo can outgrow them (server-wide chat on a
large server) leave the ratchet tree out (section 4, "Large groups").

### Delivery service requests

All binary fields are base64url. Response objects reject unknown fields when
decoded in Rust; in particular, the required `next` field distinguishes an
MLS `{groups, next}` page from the non-MLS `{groups}` role list, including
when both lists are empty. The generated TypeScript wire shapes stay the same.

**Paging.** Every request that returns a list takes `after?` and `limit?`
(default and maximum 100) and returns its entries in a stable order, at most
`limit` of them, with `next` set when more follow: pass it as `after` to
continue. The complete authenticated response must fit 640 KiB. Lists use a
640 KiB minus 4 KiB item budget, including the log's pending proposals; the
remaining space covers the JSON envelope and cursor. Filters, cursors and
count limits run in SQL. Blob inboxes and logs stream rows and stop at the
count or byte budget with at most one eligible lookahead row; device and
package lists scan batches of at most eight candidates, skipping revoked
devices before counting returned entries. The order is by conversation for `mls.groups`, by
`(uid, device_key)` for `mls.devices` and `mls.key_packages.claim`, by id for
Welcomes and history chunks, by `(group, device)` for
`mls.history.wanted`. `mls.log` pages by epoch instead: `since` is
inclusive and `next` is the epoch to pass as `since` to continue.
`mls.devices` and `mls.key_packages.claim` take at most 20 uids; a claim
consumes only the packages it returns. An empty uid list returns an empty
page without scanning key packages.

| Request | Reply | Rule |
| --- | --- | --- |
| `mls.device.publish {hpke_key, created_at, signature}` | `{}` | the session's device; replaces an older one |
| `mls.key_packages.publish {packages, last_resort?}` | `{count}` | each parsed with `ds::key_package`, device and uid of the session, already valid (`not_before` not after the server's clock); at most 20 stored plus one last-resort |
| `mls.key_packages.claim {uids}` | `{packages: [{device_key, package}]}` | one per device of those uids with a package, except the caller's device; consumes a regular one, else returns the last-resort one; skips revoked devices and packages whose lifetime ends within 15 minutes |
| `mls.devices {uids}` | `{devices: [{uid, device_key, hpke_key, created_at, signature}]}` | published device HPKE keys |
| `mls.group.create {conversation, group_info}` | `{group}` | the GroupInfo of epoch 0, holding only the creator's device; `Observer::create` accepts it; the session may create it (below); first wins per `(conversation, incarnation)`, later attempts get `conflict` with the existing group id |
| `mls.group_info {group}` | `{group_info, epoch}` | the session may read the conversation |
| `mls.commit {group, epoch, commit, group_info, welcome?}` | `{epoch}`, or error `stale`, `expired`, `early` or `bad_request` | below |
| `mls.log {group, since, limit?}` | `{head, oldest, entries: [{epoch, proposals, commit}], pending}` | entries from epoch `since`, each keyed by the epoch its commit was made in (it moved the group to `epoch + 1`); `proposals` are the server proposals the commit references, to be processed before it; `pending` the server's outstanding proposals for the head epoch (see "Removal proposals"); 90 days kept |
| `mls.groups {}` | `{groups: [{conversation, group, incarnation, epoch, pending}]}` | the live group of every conversation the session may read; `pending` whether outstanding proposals exist |
| `mls.welcomes {}` | `{welcomes: [{id, group, welcome}]}` | for the session's device; repeatable |
| `mls.welcomes.ack {ids}` | `{}` | after the client stored the joined group durably; deletes them. The client stores the Welcome's id with the joined group in one write, and acknowledges a Welcome whose id it already has without processing it again (its key package is used up) |
| `mls.history.share {group, recipient, chunk, assignment?}` | `{}` | the session's device and `recipient` both have leaves in the group's current epoch (observer); the chunk parses (netstrings, header fields canonical) and its header names the server key, `group`, `recipient` with its published HPKE key, and the session's device key as sender; the signature verifies with that key. The recipient must differ from the sender's own device, and `last_epoch` must not exceed the group's current epoch. In one write transaction the server checks these rules and the SHA-256 of the chunk bytes: the same hash for the same group and recipient is idempotent (`{}`, no new row or broadcast), even at quota. At most 64 stored chunks per `(group, recipient device, sender_uid)` and three distinct senders per recipient/group are allowed; exceeding either returns `quota_exceeded` without a write. A new chunk is stored with `sender_uid` = the session's uid and ends the recipient's outstanding request in that same transaction (in channel and server groups only when it names an active assignment of that request, section 4) |
| `mls.history.fetch {group?}` | `{chunks: [{id, group, sender_uid, chunk}]}` | chunks for the session's device; repeatable |
| `mls.history.wanted {}` | `{wanted: [{group, device, assignment?}]}` | the outstanding requests (below) in groups the session's device is a member of; in channel and server groups only the calling device's active assignments, with their ids (section 4) |
| `mls.history.ack {ids}` | `{}` | after durable import; deletes them |
| `mls.history.request {group}` | `{}` | stores an outstanding request of the session's device for the group and announces it to the group's online member devices (event `mls.history.wanted {group, device}`; in channel and server groups routed by assignments, section 4); at most once a minute per device and group. A request ends when a chunk for that device and group is stored after it (in channel and server groups, a newly stored chunk answering an active assignment of it), or after 30 days |

Events: `mls.commit {group, epoch, proposals, commit}`, `mls.proposal {group,
epoch, proposal}`, `mls.welcome {group}`, `mls.history {group}`,
`mls.history.wanted {group, device}` (with `assignment` in channel and server
groups, section 4); `mls.welcome` and `mls.history` only
announce that something can be fetched.

### Commit ordering

SQLite uses WAL with `synchronous=FULL`. Every mutation's success reply and
MLS broadcast follows its committed transaction; the commit clock is also
committed before validation can return `expired`. Transaction failures publish
nothing and leave the stored group available for retry.

`head` is the group's current epoch and `oldest` the epoch of the oldest
entry the log still has, or `head` when it has none.

The core actor handles `mls.commit` atomically. The commit must be for
`group` at exactly the stored epoch, else `stale`; then `Observer::commit`
must accept it, else `expired` or `early` (`DsError::Expired` and
`DsError::Early`; the former is checked before anything
else in the commit) or `bad_request`. Key packages are claimed with at least
15 minutes left (clients add none with less than 10), so a commit's adds do
not run out while it can still be accepted. An external join (`NewMember`) is allowed
only for a session whose uid may read the conversation, and its new leaf must
be that session's device. `Observer::commit` works on a copy loaded from the
stored state: only after the transaction that stores the commit (with the
server proposals it references), the new GroupInfo, the observer state, the
leaf map and the Welcome has succeeded does the actor keep the new observer
(or simply reload it next time). Then `mls.commit` goes to every session that
may read the conversation, and the Welcome to its recipients.

### Removal proposals

The server proposes `Remove` (signed by its key, via
`Observer::propose_remove`) for every leaf of a device that: was revoked
(Connect feed), belongs to a member who was removed from the server or banned,
or (direct messages) does not belong to a participant. A proposal is stored
in `mls_proposals` together with the observer state that holds it (one
transaction, so a later commit can reference it) before it is sent as
`mls.proposal` to sessions that may read the conversation, and stays
outstanding until a commit advances the epoch; the transaction that stores
that commit issues and stores new proposals for every such leaf still in the
tree. The server also reconciles every group at startup and after each
revocation, member removal or ban (including from the CLI while it was
stopped), proposing what is missing. Reconnecting devices find
outstanding proposals through `pending` in `mls.groups` and `mls.log`. Any
member commits them; the server only checks that a commit is valid and in
order. A proposal a commit references is stored with that commit's log
entry.

### Who may do what

- A **member** here is a server member who is not banned.
- Direct message `dm:{a}:{b}`: a session of `a` or `b` that is a member of
  the server may read the conversation and maintain its group (GroupInfo,
  log, commits including external joins, Welcomes, history, sealed history,
  read marks), whether or not the other participant still is a member.
  Creating the group and writing (send, edit, delete) also need the other
  participant to be a member. The creator is one of them.
- Channel and server groups (step 4): sessions that can read the channel, or
  every member.

### Storage (server)

New tables (migration in `store.rs`):

- `mls_devices(device_key, uid, hpke_key, created_at, signature)`;
- `mls_key_packages(ref, device_key, uid, package, last_resort, not_after)`;
- `mls_groups(group_id, conversation, incarnation, epoch, committed_at,
  group_info, observer, created_at)`; one live group per conversation, the
  highest incarnation;
- `mls_log(group_id, epoch, proposals, commit, committed_at)`;
- `mls_leaves(group_id, leaf, device_key, uid)`;
- `mls_welcomes(id, device_key, group_id, welcome, created_at)`;
- `mls_proposals(group_id, epoch, leaf, proposal)`, the outstanding server
  proposals;
- `mls_history_requests(group_id, device_key, requested_at)`;
- `mls_history(id, group_id, recipient_device, sender_uid, first_epoch,
  last_epoch, chunk, created_at)`;
- `sealed_messages(id, conversation, group_id, author_uid, author_name,
  message_id, revision, epoch, ciphertext, sent_at, edited_at, deleted)`, with
  the server's numeric message id as today, unique `(conversation,
  message_id)`;
- `conversation_reads(user_id, conversation, message)`;
- `sealed_message_identities(conversation, message_id)`: permanent replay
  reservations, as for plaintext message identities, retained when a body is
  pruned or its author removed. Reserving an id, inserting the sealed body and
  pruning to 1000 happen in one transaction. Numeric sealed ids never repeat,
  including after all bodies have been deleted, so existing read marks remain
  meaningful.

Expired key packages, and log entries, Welcomes and history chunks older than
90 days are pruned with the existing periodic cleanup. Groups of a direct
message are never deleted: a participant removed from the server loses their
devices' leaves by removal proposals, and if they come back, their devices
join the same group again. A deleted channel takes its groups with it (a new
channel is a new conversation).

Removing a member (`member.remove`, pruning) deletes their read marks; with
`delete_messages` it also deletes their sealed messages, in the same
transaction, announced as `chat.sealed.removed {conversation, ids}` to the
sessions of the other participant. Merging members does not reassign sealed
messages: like signed messages, their author is inside the signature.

## 3. Encrypted direct messages

Status: core, delivery service, browser runtime, and web client/UI implemented.
The local `pnpm --filter web e2e:dm` suite exercises four simulated devices,
offline delivery, history recovery, edits, deletions and revoked-device isolation.
Encrypted attachments, channels and voice remain subsequent steps.

### Conversation and group

- Direct messages between two Gwar members are always end-to-end encrypted.
  Direct messages with TeamSpeak users stay plaintext, live-only and labeled
  "Not end-to-end encrypted", as today. The server rejects a plaintext
  direct message unless its sender or its recipient is a TeamSpeak user
  (messages from TeamSpeak arrive through the bridge), and clients drop a
  plaintext direct message unless one of its two participants is a
  TeamSpeak user. Such a conversation is `dm_conversation(uid, ts_uid)`
  with the TeamSpeak participant's `ts:…` id. The verifiers (`vc-proto`
  `verify_message`, web `verifyMessage`) stop splitting a direct message's
  conversation on `:` and instead compare it with `dm_conversation` of its
  two known participants, the author being one of them and every Gwar
  participant a valid uid; a Gwar author's message to a TeamSpeak user is
  signed and verified as before, and a TeamSpeak user's message is unsigned.
  This fixes today's signed Gwar-to-TeamSpeak messages, which fail
  verification.
- Opening a conversation: `mls.groups` already lists it, or the client creates
  it: a descriptor (kind `dm`, incarnation 0, history 1), then agent `create`
  with its own leaf, persisting the tentative **Creating** group before
  registering it with `mls.group.create`, then `settle` with the group of the
  reply `{group}` or of `conflict`. If it won: `mls.key_packages.claim {uids:
  [peer, self]}` and one `mls.commit` adding every claimed device not yet in
  the group and current in the local revocation view, with its Welcome. Apply
  this filter both to advertised admission candidates and to claimed packages,
  since the local view may be ahead of the server's. If creation lost, join the
  winner by Welcome or external commit. A group
  still **Creating** after a restart is settled with `mls.groups`.
- Before treating an unfamiliar Welcome as superseded by local active
  membership, catch up through the authoritative log, stopping at eviction.
  If pruning prevents replay to eviction, try the readmission Welcome before
  external rejoin: it may replace stale active/joining membership only when it
  authenticates the same group at a strictly newer epoch and passes the shared
  Welcome acceptance rules. A successful import is acknowledged only after the
  joined state and delivery id are durable. If no usable Welcome exists, fall
  back to external rejoin; skipped epoch keys may require history sharing.
  An already recorded id is deduplicated.
- Devices without a key package (never online since the update), and new
  devices, join on connect by external commit, in the background (design:
  "Joining").
- History: after processing an accepted commit that admits a device (an add
  or an external join) in a group whose descriptor says `history = 1`, every
  member device that holds epoch keys queues a share for that device in its
  outbox and sends it, retrying after reconnects until the server accepted
  it. The server keeps chunks from at most three sender uids per (group,
  recipient) and drops further senders. A device missing epoch keys after
  importing what it received calls `mls.history.request`; member devices
  answer the `mls.history.wanted` event, live, or the `mls.history.wanted`
  request after connecting, by sharing again (by their own evidence, see "History
  bundles"); a request ends when any share for that device is stored, and the
  device asks again if keys are still missing. Until keys arrive, a message the device cannot open
  says "Earlier messages will appear when another member is online."

### Messages

| Request | Reply | Rule |
| --- | --- | --- |
| `chat.sealed.send {conversation, group, epoch, message_id, ciphertext}` | `SealedMessage` | the session may write the conversation; `group` is its live group and `epoch` its current epoch, else `stale`; no outstanding server proposal, else `pending`; `message_id` unique in it; ciphertext at most 24 KiB |
| `chat.sealed.edit {conversation, group, epoch, message_id, revision, ciphertext}` | `SealedMessage` | only the author's uid; the same `stale` and `pending` rules; `revision` above the stored one |
| `chat.delete_own {conversation, group, epoch, message_id, revision, ciphertext}` | `SealedMessage` | the signed deletion revision, with the same rules; stored with `deleted`, no further edits |
| `chat.sealed.history {conversation, before?, limit?}` | `{sealed: [SealedMessage]}` | as channel history |
| `chat.read {conversation, message}` | `{}` | read marks per conversation |

Before writing, the server reconciles that group's removal proposals; a
storage failure refuses the write rather than using an epoch still known to a
revoked device. Sealed writes are also refused with `pending` while the group has an
outstanding server proposal (see "Removal proposals"). On `stale` or
`pending` the client catches up (`mls.log`), commits outstanding proposals
if needed (a device whose own leaf an outstanding proposal removes cannot
commit it: if it may still read the conversation, as after a ban was lifted,
it rejoins by external commit, replacing its old leaf; otherwise it stops),
checks that the group is not blocked, then signs the message again
with the new `group` and `epoch` (same `message_id`, revision and predecessor
hash) and encrypts it with the new epoch's key. A write is never accepted
under an epoch a removed device still knows.

`SealedMessage {id, conversation, group, epoch, author_uid, author_name,
message_id, revision, ciphertext, sent_at, edited_at?, deleted}`; event
`chat.sealed` for new messages, edits and deletions, to the sessions of both
participants. The server stores the latest revision only (as today), keeps
at most the newest 1000 per conversation, and counts unread messages per
conversation for each participant (no mentions: they are inside the
ciphertext). A participant without a read mark has read nothing in that
conversation (unlike channels, where a missing mark starts at the newest
message). `Welcome` gains `conversations: [{conversation, last_read, count}]`
for every direct message with unread messages (counts capped at `UNREAD_CAP`,
100, as for channels), and the `chat.read` event gains
the conversation form, for the user's other sessions. A read request or event
has exactly one of `channel` and `conversation`, plus `message`. Marks only
move forward. Sending and editing do not advance a mark; retained deletion
rows count until read, and an edit keeps the original numeric id.

Message ids are canonical base64url encodings of 16 bytes, and ciphertext is
canonical unpadded base64url. The 24 KiB limit applies to decoded ciphertext,
including edits and deletions. The server treats it as opaque: the encrypted
signature, inner revision and deletion body are verified by receiving clients.
It enforces the outer author uid, increasing revision, and terminal `deleted`
state. Timestamps are server receipt times, with `sent_at` unchanged by edits.

Sealed history uses channel history's `before` cursor and oldest-first order,
default 50 and a limit clamped to 1–100. It can return fewer rows to keep the
complete response within 640 KiB; continue using the oldest returned id as
`before`. Sealed events and conversation read events never go to the TeamSpeak
bridge.

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
  the wrong uid, removal proposals after revocation and member removal (and
  their replay to a device that was offline), outstanding history requests,
  `stale` sealed writes, paging with `next`, a removed participant's sealed
  messages, plaintext direct messages between Gwar members rejected, size
  limits, pruning.
- Client: a Welcome delivered again after a crash is acknowledged, not
  processed.
- Web: two simulated devices per user against a real server in Playwright: a
  message to an offline user arrives after they connect; a second device sees
  history; a revoked device is removed and cannot read new messages.

## 4. Encrypted channels and server-wide chat

Status: 4a server side built (delivery service, history routing, TeamSpeak
bridge); the agent and the browser runtime keep channel groups (create, join,
history assignments, leaf updates, deleted channels); the web client reads and
writes encrypted channels (create options, sealed chat, gap lines, mention
scan and accounting, moderator removal, "Who can read"); restarts are built,
for direct messages and channels; encrypted attachments are built (4b), in
channels and direct messages. Built as three pull requests, in this order: **4a**
channels (options, groups, sealed channel messages, unread counts and
mentions, TeamSpeak text isolation, restarts); **4b** encrypted attachments,
in channels and direct messages; **4c** encrypted server-wide chat and large
groups. Voice in an encrypted channel stays as it is until step 5; until
then channel info says "Messages are end-to-end encrypted", not more.

### Channel options

- `Channel` gains `encrypted` and `history` (booleans), fixed at creation:
  `channel.create` takes both (`encrypted` defaults to false, with or without
  TeamSpeak compatibility: encryption is chosen per channel; `history`
  defaults to true and is ignored, stored as false, when `encrypted` is
  false). `channel.update` cannot change
  them. The server's copy is for display and access; clients take `history`
  only from the descriptor.
- An encrypted channel has no password: `channel.create` and
  `channel.update` reject `password` for it (`bad_request`). Passwords remain
  for plaintext channels.
- `ServerInfo` gains `teamspeak` (TeamSpeak compatibility is on) and
  `server_chat: {encrypted, history}`, so clients know what is bridged and how
  server chat is kept.
- With TeamSpeak compatibility the default channel stays plaintext:
  `server.update` refuses an encrypted `default_channel` (`bad_request`), and
  the server refuses to start with TeamSpeak compatibility while the stored
  default channel is encrypted (see "Server-wide chat").
  Channels created by a new server's database setup are plaintext.
- Existing channels keep `encrypted = false`; nothing is converted.
- The create dialog shows "End-to-end encrypted" (off by default, the
  creator's last choice remembered per server) and, when it is on, "New
  members see earlier messages", both marked as permanent. Channel info shows
  "Messages are end-to-end encrypted" with a small lock, or "Not end-to-end
  encrypted" in muted text, and lists **who can read**: the devices in the
  channel's group, grouped by member.

### Groups

- Conversation `channel:{id}` (kind `channel`, `private = 0`, `history` from
  the channel), one group per incarnation. Any member may create it
  (`mls.group.create`: the descriptor's kind, conversation, `history` equal to
  the channel's, an encrypted channel that exists, creator a member); the
  first creation wins, as for direct messages. The client that created the
  channel creates the group right away; any device that finds an encrypted
  channel without a group (`mls.groups` does not list it) creates it, after
  a random delay of up to five seconds, and settles as in section 3.
- Who may read: every member, for every encrypted channel. Reading covers
  the group (GroupInfo, log, commits, external joins, history) and the sealed
  messages; writing needs the same. The membership rule is the descriptor's
  kind: any well-formed device (the server's word).
- **Joining**: after connecting, a device joins, by external commit, the
  group of every encrypted channel it is not in, at most two at a time, with
  a random delay of up to two seconds between joins and backoff on failure.
  A restart the device itself makes is not a failure: it keeps its new
  group current without waiting.
- **Leaving the server** removes the member's devices from every channel
  group by server proposals, as for direct messages ("Removal proposals").
  Deleting a channel deletes its groups, log, Welcomes, history chunks and
  sealed messages (with their files) in one transaction, and announces
  `channel.deleted` as today; clients delete the group's MLS state and keep
  nothing of it.
- **Leaf updates**: each device commits an update of its own leaf in every
  channel and server group it is in when its last own commit there is older
  than seven days (checked after connecting and once a day while
  connected), one group at a time.

### History in channels

Channels can have many member devices online, so sharing after an admission
is driven by the newcomer and routed by the server, instead of every member
device sharing on its own (section 3, which stays as it is for direct
messages):

- After joining a channel or server group, the new device calls
  `mls.history.request {group}` (whatever the group's `history`, and whether
  or not it had a leaf before: senders decide by their own evidence), and
  again, as in section 3, while rows it has fetched stay unopenable after
  importing what arrived.
- The server routes each outstanding request by **assignments**, kept in
  memory (a restart of the server starts routing afresh from the stored
  requests). An assignment has a random id and asks one member device that
  has a leaf in the group's current epoch and is online:
  `mls.history.wanted {group, device, assignment}`. At most three are active
  per request, preferring distinct uids and the most recently connected
  sessions, and only while the recipient device still has a leaf.
- In channel and server groups `mls.history.share` carries the
  `assignment` it answers, and `mls.history.wanted {}` lists
  `{group, device, assignment}`. An active assignment ends exactly once, by
  the first of: a chunk naming it stored by that device for the recipient;
  `mls.history.decline {group, device, assignment}` (nothing to share); its
  device disconnecting; one minute passing. Only a chunk naming an active
  assignment of the request ends the request and counts as that device's
  turn. Chunks naming an ended or unknown assignment (the rest of a
  multi-chunk answer, or a delayed one after a timeout, a rejoin or a server
  restart) are checked and stored by the usual rules and quota, so their
  keys still arrive, but change no request or routing; neither do stale
  declines.
- The server keeps a **rotation** per `(group, recipient device)`, across
  that recipient's requests: the devices **attempted** in it. Each device
  gets at most one assignment per rotation; ending by a chunk, a decline, a
  timeout or a disconnect uses up its attempt. Only an assignment cancelled
  because a chunk ended the request does not: that device can be asked again
  by the recipient's next request. While a request is outstanding, every
  ended assignment and every newly eligible session leads to the next
  announcement, to a device not yet attempted, preferring distinct uids and
  the most recently connected. A stored chunk ends the request as in section
  2 and cancels its other assignments, but does not reset the rotation, so
  the recipient's next request reaches devices that have not been asked yet
  (an older key holder after a sharer with recent keys only). When every
  eligible device has been attempted and no assignment is active, the
  rotation starts over, every device eligible again, but no earlier than a
  backoff after the previous start (one minute, doubling up to an hour).
  Rotations live in memory and are dropped when no request of that recipient
  was made for 30 days. Removing the recipient's leaf from the group cancels
  its request and assignments and drops its rotation in the same step.
- `mls.history.wanted {}` (the request) lists only the calling device's
  active assignments. Direct messages keep section 3's behaviour: every
  member device shares after an admission and answers every announced
  request.
- A device answers an assignment as in section 3, by its own evidence, or
  declines. The quota (chunks from three sender uids per recipient and group)
  and acknowledgements are unchanged.
- Rows from epochs the device still has no key for show, in a group with
  `history = 1`, "Earlier messages will appear when another member is
  online"; in a group with `history = 0`, rows from before the device's
  first epoch are collapsed into one muted line, "Earlier messages aren't
  shared with new members".
- **Evidence** is stored per group as intervals instead of per-epoch lists:
  for every uid, the ranges of epochs in which it had a leaf, as the device
  saw them. The agent records it after every merge and join; the rule "only
  epochs it derived whose evidence lists the recipient's uid" reads the
  intervals.

### Messages

The sealed message requests of section 3 apply to `channel:{id}` and to
`server` (4c) with these changes:

- Access: as under "Groups" (writing a channel needs it to exist and be
  encrypted). Plaintext `chat.send`, `chat.edit` and `chat.history` refuse an
  encrypted channel (`bad_request`), and the sealed requests refuse a
  plaintext one.
- `chat.sealed.remove {conversation, id}` (moderators with `message_manage`)
  removes a sealed message of a channel or of server chat (not of a direct
  message) without a
  signature, as `chat.delete` does for plaintext ones, and announces
  `chat.sealed.removed {conversation, ids}`. Authors keep using
  `chat.delete_own`.
- Events: `chat.sealed` and `chat.sealed.removed` go to every session that
  may read the conversation.
- Numeric ids come from the same counter as direct messages' sealed ids. The
  newest 1000 messages per conversation are kept, as for plaintext channels.
- **Read marks** use the conversation form (`chat.read {conversation,
  message}`). For channel and server conversations a missing mark is set to
  the newest sealed message the first time the server computes the member's
  unread count (their Welcome), as for plaintext channels, so a newcomer does
  not start with the whole history unread; direct messages keep starting at
  zero.
  `Welcome.conversations` also lists every encrypted channel conversation the
  member can read, with its read mark even when nothing is unread (mention
  counts are pruned at it); `Welcome.unread` keeps listing plaintext channels
  only.
- **Mentions** are inside the ciphertext. After connecting, for every
  conversation with unread messages the client pages backwards through the
  unread rows (`chat.sealed.history`, continuing after byte-short pages) until
  its read mark, the oldest retained row (at most 1000), or `UNREAD_CAP`
  verified messages mentioning it, and opens them to count those mentions,
  without notifying; at most four conversations at a time.
- **Mention accounting.** The client keeps one mention contribution per
  conversation and authenticated message identity, from its latest accepted
  revision. History opening, live delivery and retries after a missing key
  set that contribution idempotently; an edit replaces it, a removal clears
  it. Removals are remembered (tombstones) until outstanding scans and opens
  for that conversation finish, and every result is checked against the
  current read mark before it is applied. If a scan stopped at `UNREAD_CAP`
  and the count later falls below it (an edit or removal), the client scans
  again until the cap, its read mark or the oldest retained row.
  The server cannot check mentions; receiving clients ignore mentions of uids
  that are not members they know.
- **Notifications** use the existing settings (`mentions`, `allMessages`, own
  messages and the focused conversation suppressed), decided after the
  authenticated opening. Only a live original message (revision 0) makes a
  message eligible; history, edits and deletions never do. Eligibility is
  consumed once, also when the decision is to stay silent, and survives
  retries after a missing key without notifying twice.

### TeamSpeak

What stays plaintext and labeled: channels created plaintext, plaintext server
chat, direct messages with TeamSpeak users. For text, in this step:

- Sealed requests and events never reach the bridge (as for direct
  messages). Text from TeamSpeak into an encrypted channel, or into server
  chat while it is encrypted, is refused by the core and dropped with a
  debug log; it is never stored or relayed.
- An encrypted channel exists on TeamSpeak with a join power above every
  TeamSpeak group and a fixed description, "End-to-end encrypted. Join from
  Gwar.", never its participant list. The bridge does not move or create a
  puppet in an encrypted channel: a Gwar user in an encrypted channel has no
  puppet, except one kept for direct messages with TeamSpeak users, which
  stays in the default channel and carries text only.
- Bridge text boundaries (design: "TeamSpeak and plaintext channels"): every
  queued channel text keeps the Gwar channel it came from, whether that
  channel is encrypted, and the generation of the puppet or query route it
  was queued for. Delivery checks them again: channel text is sent only by a
  puppet whose confirmed TeamSpeak channel is that channel's, and through the
  query only to that channel's TeamSpeak channel; text queued across a move,
  a failed move or a reconnect is dropped, never sent into another channel.
  The same holds from TeamSpeak to Gwar: channel text is attributed to the
  channel the TeamSpeak user was confirmed in when it arrived. Direct
  messages and server chat are classified by their own conversation, never
  by the sender's voice channel. In the code this means: `PuppetCmd::Text`
  carries the originating channel, its classification and a route
  generation that also changes on every move (today `Local.generation`
  changes only when a puppet is replaced); `puppet.rs` tracks its confirmed
  TeamSpeak channel and pending or failed moves, and channel text waits for
  a confirmed matching channel or is dropped; the query path keeps its
  awaited move to the message's fixed destination; incoming channel text
  keeps the channel id and generation captured by the listener, never
  `Remote.cid` read later.
- Server chat is bridged only while its stored mode is plaintext (below).

### Restarts

Incarnations above zero are accepted from this step on, for direct messages,
channels and server chat:

- **Trigger.** A device considers restarting only its highest accepted
  incarnation (its live pin). The trigger is an authenticated observation
  behind its durable freshness checkpoint for that group, or forking from it:
  a GroupInfo of an earlier epoch or with another GroupContext for the same
  epoch, or a log whose head is behind it. It takes two such failed checks
  against the same live pin and checkpoint, at least one minute apart. Any
  validated response at that checkpoint or continuing from it (a verified
  GroupInfo at that GroupContext or later, also when there was nothing to
  merge, or a log entry merged from it) cancels the trigger, and a later
  failure starts the wait again; a log that only reports its head at the
  checkpoint proves nothing and decides nothing; a changed pin or checkpoint discards pending checks. A server whose live incarnation is older than the device's pin
  (it lost a restart) never replaces the pin and never leads the device to
  create a sibling: the device stays blocked in that conversation ("Securing
  this conversation…") until the server again holds its pinned incarnation
  or a successor of it.
- **Creating.** The device persists one restart candidate per predecessor:
  the next incarnation (`previous_group_id` its pinned live group, the same
  kind, conversation, `private`, `history` and `founding_owner`), created as
  a **Creating** group, then registers it with `mls.group.create` and settles
  as in section 3: the server accepts only the successor of its live group
  (as implemented), first wins, and a device that lost joins the winner after
  accepting it below.
- **Accepting.** `mls.group_history {conversation, after?, limit?}` returns
  `{incarnations: [{group, descriptor}], next}`, the conversation's groups by
  incarnation, each with its complete encoded `SignedDescriptor`, under the
  conversation's read rule. A device verifies, along the chain: each
  encoded `SignedDescriptor` parses and its signature verifies, `group`
  equals its `group_id()` (the SHA-256 of the canonical descriptor statement,
  section 1, not of the signed envelope), server key and
  conversation, consecutive incarnation numbers, each `previous_group_id`
  equal to the group before, the unchanged fields above, and that each
  creator was certified at its creation time and, for a direct message, is
  one of its participants. That the creator could then create the
  conversation ("Who may do what": a member, who can read the channel) is
  the server's check at `mls.group.create`, as for incarnation zero: a
  device has no record of past server membership. An existing device anchors the chain at its highest
  accepted pin; a new device validates from incarnation zero (the
  first-contact limit of the design's "Freshness"). The accepted pins and the
  highest live incarnation are persisted in one write; a predecessor is never
  live again. Old incarnations keep their keys for reading and nobody writes
  to them; the conversation shows a quiet line "This conversation was
  restarted".
- **In the code.** `mls.groups` names each live group's `incarnation`. The
  agent keeps one record per accepted incarnation: the live pin is the
  highest that is not a creation still to settle. `check_pin` lets a device
  join or accept a Welcome only for its live pin, or for incarnation zero
  without any pin; `create` takes incarnation zero or the next incarnation of
  the live pin, one candidate per predecessor. `accept_chain(server,
  conversation, [(group, descriptor)])` verifies the chain from the live pin
  (or from incarnation zero) and records the new incarnations (the last one
  to be joined, as **Rejoin**, or settled as won when it is this device's
  own creation); `settle` of a won restart and `accept_chain`
  turn every earlier incarnation **Superseded**: its OpenMLS state, private
  epoch key pairs and outbox are deleted, its epoch keys and evidence kept,
  and the runtime's jobs for it go in the same write. `behind(server, group,
  group_info)` verifies a GroupInfo of that group as an external joiner would
  (tree, a member's signature, the rules; leaf lifetimes skipped) and reports whether it is behind the group's
  checkpoint or forks it. The browser runtime counts a log head behind the
  checkpoint, or such a GroupInfo when rejoining, as a failed check; pending
  checks live in memory, so a reload starts the wait again. A held
  conversation also imports no Welcome, shares no history and requests no
  keys. A Welcome to a
  superseded group is acknowledged without importing it. The chat shows the
  restarted line when the live incarnation is above zero, and rows of earlier
  incarnations that this device cannot open do not wait for history.

### Encrypted attachments (4b)

- A file is encrypted before upload: a random 32-byte file key, AES-256-GCM
  with a random 96-bit nonce, AAD `gwar file v1`, `ciphertext = nonce ‖ sealed
  ‖ tag`. `file.upload` gains `sealed: true`: the server stores the name
  `file` and type `application/octet-stream`, does not sniff or measure it,
  and serves it as a download of that type. Its size and SHA-256 are the
  ciphertext's; the upload limit and quotas apply to it.
- The signed body v3 carries per attachment the plaintext SHA-256, size,
  type and name, the file key, and the ciphertext's SHA-256 and size (section
  2). The plaintext JSON's attachment objects carry the same plus the file id;
  width and height, if present, are unsigned display hints: the image
  header's dimensions (what the server measures for a plaintext image,
  before any EXIF rotation), checked against the decrypted file's header.
- The upload's `sealed` flag is stored with the file. `chat.sealed.send`
  gains `files: [id]` (at most 10, distinct): sealed uploads of the session's
  uid that are finished and not attached to any message; they are tied to
  the message in the same transaction. Plaintext `chat.send` accepts only
  plaintext uploads. A file belongs to at most one message, plaintext
  (`files.message`) or sealed (a new `files.sealed_message`), never both;
  sealed and plaintext numeric ids are separate and can coincide, so every
  lookup names which one. Edits keep the files (the client checks that every
  revision but the deletion lists the same attachments).
- Every deletion path deletes a sealed message's files with it, in the same
  transaction: `chat.delete_own`, `chat.sealed.remove`, pruning past 1000,
  channel deletion, and member removal or pruning with `delete_messages`
  (including the CLI). Without `delete_messages`, member removal keeps the
  files of the messages it keeps. Every predicate that finds unattached or
  orphaned files (pending-upload cleanup, `expire_unattached`, removal)
  considers both associations. Bytes are unlinked as today: deferred, after
  active downloads end, and charged to the uploader until they are gone.
- Downloads: with `file_access = members`, a session that may read the
  conversation of the file's sealed message, resolved through that
  association; with `link`, anyone with the link, who gets only ciphertext.
- Receiving: download, check the ciphertext's hash and size, decrypt, check
  the plaintext's hash and size, then show (images from a `blob:` URL).
  A failure shows the attachment as "Can't open this file". Direct messages
  get attachments too.

### Server-wide chat (4c)

- The server's mode (`plaintext`, or `encrypted` with history) is stored in
  `meta` and never changes once set. An existing database is set to
  `plaintext` by the migration. A new database gets it at the first start of
  the server itself (`vc-server` serving, not administrative commands such as
  `admin-token`, which leave it unset): `plaintext` with TeamSpeak
  compatibility, `encrypted` without. From then on the stored mode decides:
  server chat is bridged only when it is plaintext, plaintext `chat.send` to
  the server is refused when it is encrypted, and sealed requests for
  `server` are refused when it is plaintext.
- Turning TeamSpeak compatibility on later keeps an encrypted server chat
  (not bridged). The server refuses to start with TeamSpeak compatibility
  while the default channel is encrypted, saying to choose a plaintext default
  channel first (`server.update`, without TeamSpeak compatibility).
- Encrypted server chat is conversation `server` (kind `server`), stored as
  sealed messages, with the channel rules above (read by every member, joined
  on connect, history routed by the server). Plaintext server chat stays
  live-only and bridged, as today.

### Large groups

A public channel's and server chat's group holds every device of every
member who connected since, so its ratchet tree can outgrow the 128 KiB blob
limit (about 300 devices):

- Channel and server groups leave the ratchet tree out of the GroupInfos
  uploaded with commits (no `ratchet_tree` extension); only the creation's
  GroupInfo, with its single leaf, carries it. Committers produce that
  GroupInfo explicitly (OpenMLS `create_group_info(true)` with tree inclusion
  disabled; disabling tree inclusion alone also stops OpenMLS from producing
  a GroupInfo). For these groups `Observer::commit` accepts a GroupInfo with
  exactly the `external_pub` extension and no tree, and keeps every other
  check (the observer's new GroupContext and confirmation tag, a member's
  signature). Creation, and direct messages throughout, keep today's checks. Joiners fetch the tree of the current epoch
  over HTTP: `GET /api/mls/{group}/tree`, with the session's file access
  token as `Authorization: Bearer …`, answered with the TLS-encoded ratchet
  tree the observer holds and `x-gwar-epoch`; 404 if the session may not
  read the conversation. The client builds the external commit with that
  tree, whose hash OpenMLS checks against the GroupInfo; if the epoch
  changed meanwhile, it fetches both again.
- Direct messages keep the tree in GroupInfos and Welcomes.
- Commits stay small (a path, not the tree), so the WebSocket limits of
  section 2 stay as they are.

### Tests

- `gwar-mls`: restart acceptance (successor fields, chain, creator, a new
  device from incarnation zero, a server behind the pin creating no
  sibling), evidence intervals, external join with a tree supplied
  separately, `Observer::commit` with a tree-free GroupInfo in channel groups
  and refusing one in direct messages.
- Server: channel options and their refusals (password, update, default
  channel, starting with TeamSpeak compatibility while the default channel is
  encrypted), group creation rules, plaintext/sealed refusals both ways,
  `chat.sealed.remove`, channel deletion cleanup, read marks for channels,
  history routing (three assignments, decline, timeout, disconnect, a newly
  connected device, a timed-out device asked again after reconnecting, a
  decline or chunk for an ended assignment ending nothing (the chunk still
  stored), devices that keep timing out not keeping another one waiting, a
  rotation of unresponsive devices reaching its backoff, the recipient removed while
  assignments run, a full round of declines backing off, and a recipient
  whose first sharer had only recent keys reaching an older one), `mls.group_history` paging and access, file ties
  and access,
  tree endpoint access and epoch, TeamSpeak text refusal into encrypted
  conversations, no puppet in an encrypted channel, fixed description and
  join power; queued channel text dropped across a move, a failed move and a
  reconnect; uploads of the other mode refused, a file reused, plaintext and
  sealed messages with the same numeric id; member removal with and without
  `delete_messages` and unattached-file expiry keeping or deleting the right
  files; server chat mode after migration, after `admin-token` on a new
  database, and when TeamSpeak compatibility is turned on later.
- Web (Playwright, two users with two devices each): create an encrypted
  channel, both join, messages, edits, deletions, a newcomer with and without
  history, mentions counted after opening (also a mention only in the oldest
  of 101 unread messages; a scan overlapping a live edit; a removal while
  opening; an edit or removal after stopping at 100 of 101 mentions), notifications not repeated after a delayed key or
  an edit, an encrypted attachment opened on the other device, server chat in
  4c.
