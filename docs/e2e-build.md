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
5. **Encrypted voice**: **5a** call-scoped MLS groups, protocol and server;
   **5b** opaque SFU payloads, bound SSRCs and sender clocks; **5c** the web
   SFrame worker and interface; **5d** the native engine. TeamSpeak audio
   isolation and S6 with the official server run in CI.
6. **Private channels**: **6a** hidden channels with server-enforced access
   (roles and members); **6b** the server owner chain and the signed manager
   list; **6c** encrypted private channels whose member list is signed and
   enforced by the group.
7. **Key archive** on Gwar Connect.
8. **Pre-release reviews**: the WebCrypto SFrame worker; `openmls_rust_crypto`
   and our storage.

Parts 1–6 are specified below. Voice comes before private channels and the
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
- **Private channels can also be plaintext** (step 6). A private channel is
  hidden from everyone it does not admit. Plaintext private channels admit
  by roles and listed members, on the server's word; encrypted ones admit
  only a signed member list kept in the group. The bridge does not mirror
  private channels to TeamSpeak.
- **Confirmers.** The owner may mark managers who may confirm other
  managers. A manager added by any admin then waits for the owner or a
  confirmer, not for the owner alone (answers the open question in
  [e2e.md](e2e.md)).
- **The owner chain's genesis is unsigned**: the first owner is the server's
  word, pinned on first contact like the server key. Every later link is
  signed by the previous owner's account key.

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
  (gzip budget 500 KB, checked in CI).
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
channels and direct messages; large groups are built (4c: channel and server
GroupInfos without the tree, the tree endpoint, joining with it); encrypted
server chat is built on the server and in the web client (4c complete). Built as
three pull requests, in this order: **4a**
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
  `admin-token`, which leave it unset): `plaintext`, with or without
  TeamSpeak compatibility, unless that start passes
  `--encrypted-server-chat` (`VC_ENCRYPTED_SERVER_CHAT`), which makes it
  `encrypted`. The option is read only while the mode is unset; given
  later on a server whose stored mode is `plaintext`, it logs a warning and
  the mode stays (without the option, an encrypted server stays encrypted). From then on the stored mode decides:
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
  read the conversation; 429 past 30 requests a minute from one session (the
  server exports each group's tree once per epoch). The client reads at most
  8 MiB within 30 s and builds the external commit with that
  tree, whose hash OpenMLS checks against the GroupInfo; if the epoch
  changed meanwhile, it fetches both again.
- Direct messages keep the tree in GroupInfos and Welcomes; agents ignore
  separately supplied trees for them. Channel/server devices join externally;
  key-package Adds and Welcomes are refused. The tree endpoint also serves
  direct messages to their authorized readers, with `Cache-Control: no-store`.
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

## 5. Encrypted voice

Status: **5a–5d built**. Voice MLS rules, volatile calls, bound negotiation,
opaque SFU forwarding, origin-preserving TeamSpeak queues, browser SFrame and
native SFrame are implemented. Browsers that pass the transform capability
check and the desktop app join encrypted voice. Plaintext voice and TeamSpeak
keep their existing media formats. See the [5a report](voice-calls-5a.md),
[5b report](voice-sfu-5b.md), [5c report](voice-web-5c.md) and
[5d report](voice-native-5d.md) for coverage, validation and acceptance still
pending.

RTP admission binds the negotiated microphone SSRC and retains retired SSRCs.
Native capture positions drive RTP time; per-source generations and previous
packet spans protect SFU re-anchors. Native WebRTC keeps 10 ms packets and
browsers keep 20 ms. The remaining steps add client SFrame without changing
packetization. Official-server isolation execution belongs to CI; compilation
and local boundary tests alone do not establish that acceptance.

Four pull requests, in order:

- **5a:** voice rules in `gwar-mls` (agent and observer), volatile call state,
  protocol, session admission and removal, acknowledgements and restart rules.
  Include the small web/native adapter changes for the new bindings and
  plaintext negotiation. Encrypted channels refuse audio until their client
  has SFrame; show "Encrypted voice is being added. Chat is available."
  Keep plaintext voice and TeamSpeak working. There is no temporary plaintext
  fallback in an encrypted channel.
- **5b:** SFU stream admission, opaque packets, timestamp forwarding with
  unchanged client packetization, and TeamSpeak audio boundaries. Enable neither
  client to send encrypted voice yet. Run the official-server isolation tests.
- **5c:** web worker, MLS-to-media controller, activation/retention rules,
  browser tests and "Who can hear". Enable encrypted voice for web engines
  that pass the transform capability check; desktop stays on the preceding
  calm unavailable state until 5d.
- **5d:** Rust SFrame, native key-install command, native/browser interop and
  latency acceptance. Enable encrypted voice in the desktop engine.

The adapter work in 5a/5b is deliberate: a server-only wire or SSRC change
would break the clients between merges. Each PR retains plaintext and
TeamSpeak regression coverage. This section pins down the voice installation,
retirement, per-SSRC transport state, variable-duration native playback and
origin-preserving bridge queues listed under the design's "Left for the
implementation spec"; that list remains in the design.

### Calls and membership (5a)

- **Lifetime.** Every accepted channel entry uses one call/participation
  allocation path: `channel.join`, bridge `Join`/`Update` moves and forced
  moves. The first entry into an empty channel starts its call; the last
  session leaving, moving, disconnecting or being removed ends it, as does
  channel deletion. Muting, deafening and engine restart do not leave.
  Allocate a random 16-byte `call` per lifetime and a separate random 16-byte
  `participation` per entry, using a cryptographic RNG, in plaintext too.
  TeamSpeak participants get both tokens with `run = null` and keep a
  plaintext call alive; they never enter encrypted channels. Leaving
  invalidates participation before routing changes. Re-entry gets a new
  token; a repeated join/update in the same channel changes neither token.
- **Voice group.** An encrypted call has at most one MLS group, conversation
  `voice:{channel_id}`, kind `voice`, incarnation 0, no predecessor,
  `private = 0`, `history = 0`, no founding owner. `channel_group_id` is the
  live, locally accepted channel group; `creation_nonce` is a fresh random
  16-byte client nonce for this creation candidate, independent of the call
  token (two devices of one uid can create in the same millisecond). The
  request binds the candidate to the call. The creator signs this descriptor
  normally. No voice
  incarnation chain is created: the next call has a fresh nonce and group.
  Plaintext calls have no MLS group, keys or acknowledgements.
- **Admission.** Before entering encrypted voice the device joins and catches
  up its channel group. The server requires an unbanned Gwar session, channel
  join permission, and that exact `(uid, device_key)` in the channel group's
  current observer tree; TeamSpeak sessions cannot enter. Channel capacity
  still applies. The encrypted-call limit is 1000 sessions and 1000 occupied
  leaves, including pending admissions; `client_move` does not bypass it.
  Plaintext retains its existing channel and server capacity rules.
  A listener without a microphone is a participant and has a leaf too.
- **Creation and join.** Get `voice.state`, create the epoch-zero singleton
  with the descriptor above if `group` is null, and submit
  `mls.group.create`. First creation wins; identical retries return its id,
  another candidate gets `conflict` with that id in the existing error
  message field. A creator settles **Creating** against `voice.state` before
  exporting anything. A loser discards only its tentative candidate and
  joins the winner. Join by `mls.group_info`, the tree endpoint and external
  `mls.commit`; never by a key-package Add or Welcome. A joining group stays
  non-send-capable until acceptance is reconciled through the reply/log.
- **Pin.** Keep one group pin per `(server_key, channel, call)` in memory.
  Once settled, another group id for that call is refused, even if the server
  calls it a restart. A different `call` for the same channel/participation
  without a confirmed leave or move stops the engine and leaves voice; it
  never silently replaces the pin. On a confirmed leave, move or disconnect
  the pin and all call state are discarded; a new participation can then
  accept the next call. A lost creation reply is resolved by
  `voice.state`; a lost commit reply by its hash in `mls.log`, as in section 2.
- **Leaf mapping.** The core owns `leaf -> {uid, device_key, session,
  participation}`. Creation and external commit bind the new leaf to their
  authenticated requesting session, never to a supplied uid/session. A member
  commit must come from that leaf's bound participation. One current leaf per
  device, one participation per session, one media run per participation.
  Step 5a implements takeover keyed by authenticated `(uid, device_key)`
  (`hello_key`, including local identities). After the new connection passes
  authentication/admission checks, close the old session with
  `LeaveReason::Replaced` through the common removal path before admitting
  the new one. Server capacity counts the replacement once, so a full server
  permits takeover; a rejected connection never evicts the old one. Release
  its channel/participation slot first, but count detached occupied leaves
  until removed or replaced. An external rejoin may replace only its own
  earlier leaf under section 1's rule; the fresh mapping counts as a removal.
- **Leave and removal.** Retire that participation's stream, stop routing to
  and from it, clear its acknowledgements and detach its leaf mapping in one
  actor transition. Propose `Remove` for every detached leaf while any
  participants remain. Revocation, expiry, ban, removal from the server or
  channel group use the same path. Reconcile proposals after each voice or
  parent-channel commit, on revocation updates, at the next leaf certificate
  expiry (a core timer), and before admitting a voice operation; reissue at
  the new epoch if a competing commit left the leaf in the tree. Remaining
  devices commit removals using the existing ordered flow. No server-held
  media key or server-generated member commit is introduced.
- **Parent changes.** Ordinary parent epochs do not rekey voice by themselves.
  A client holds sending while a voice leaf is absent from its local parent
  tree, has a non-current certificate, or has a pending server removal.
  On a missing leaf, synchronize the pinned parent first: propose/commit
  removal only if it is still absent after sync
  completes with no outstanding parent changes, and recheck immediately
  before submitting. This applies at admission and throughout the call;
  a lagging parent agent must not remove a newcomer. Catch-up still accepts
  structurally valid historical voice commits; current parent membership
  gates sending, not that history. Creation/join exposes no media keys until
  this gate passes. A channel-group restart closes its call
  (`channel_restarted`), moves its sessions out of voice and requires a fresh
  join tied to the successor. Do not mutate the
  voice descriptor or accept two parent groups for one call.
- **Rules and state separation.** Add explicit voice paths in `rules.rs`,
  `Agent` and `ds::Observer`: the descriptor fields above; ordinary credential,
  immutable-context, external-sender and commit-time checks; external joins
  only; no Add/Welcome, chat, history sharing/import, evidence archive or
  `accept_chain` for voice. Use the parent's validated channel rules (public
  in step 5; step 6 adds private admission). Check current parent membership
  on the client and server, without treating a server roster as proof. Voice
  uses a separate in-memory agent/provider store under the same device lease;
  it never enters IndexedDB, the chat outbox, Gwar Connect or key packages.
  Serialize its tentative state and commit-hash outbox in that store before
  emitting requests. Runtime/worker loss forces rejoin/update as below;
  chat's durable state and locks are unchanged.

### Wire and delivery (5a)

All new operations use the existing authenticated `vc/4` request/reply/event
envelopes and generated Rust/TypeScript types. Binary fields are canonical
unpadded base64url: `call` and `participation` 16 bytes; `group`,
`channel_group` and `device` 32 bytes. `channel`, `leaf` and `ssrc` are u32
(`ssrc` nonzero); `session` uses `SessionId`; `run` is an integer 1 through
`2^53 - 1`, increasing within a participation. Epochs in these wire messages
and reused MLS operations remain exact JSON integers, 0 through `2^53 - 1`;
reject fractions, negative or unsafe values, never truncate. Full epochs are
u64/BigInt internally. A call that would advance beyond this wire limit is
closed with `epoch_exhausted`, not wrapped. The low-32-bit SFrame epoch wraps
normally and is tested independently of this unreachable-in-practice limit.

`VoiceState` is `{channel, call, participation, encrypted, channel_group,
group, epoch}`. The last three fields are nullable: all null in plaintext;
in encrypted voice `channel_group` is required, and `group`/`epoch` are both
null until creation, then both present. It describes only the caller's
current participation, not authority to derive keys. Add nullable
`voice_binding: {call, participation, run}` to `Client` snapshots and updates;
null outside voice, with null `run` before transport or for TeamSpeak. Publish
it on entry, exit and run changes before any talking event for that binding,
clearing old talking state on retirement.

| Request | Reply | Rule |
| --- | --- | --- |
| `voice.state {}` | `{voice: VoiceState or null}` | current participation, or null when outside voice; read-only, also used after a lost reply |
| `mls.group.create {conversation, group_info, voice: {call, participation}}` | `{group}` | `voice` required only for voice creation and rejected for other kinds; current participation, descriptor channel/parent match, creator is its device; one group per call |
| `mls.commit {group, epoch, commit, group_info, voice: {call, participation}}` | `{epoch}` | `voice` required only for voice commits; no `welcome`; mapped member or same-device external join; existing `stale`, `expired`, `early` and validation rules |
| `voice.offer {channel, call, participation, run, group, ssrc, sdp}` | `{call, participation, run, sdp}` | replaces the existing offer shape; `group` null only in plaintext, otherwise the pinned live voice group and the caller's mapped leaf required; SDP at most 64 KiB; exactly one offered microphone SSRC, equal to `ssrc` |
| `voice.epoch.ack {call, participation, run, group, epoch}` | `{}` | current run of the mapped device, epoch exactly the observer head; emitted only after decryptor installation |
| `voice.epoch.repeat {call, participation, run, group, epoch}` | `{}` | same binding and epoch check; requests another ack from each participant, including self |

Events below go only to the relevant call's Gwar sessions, except the
existing server-wide talking display event as specified below:

- `voice.state {voice: VoiceState or null}`: per-session snapshot on enter,
  leave or group creation. Epoch advances still use `mls.commit`; do not
  broadcast 1000 individual state snapshots per commit.
- `voice.epoch.ack {call, group, epoch, device}`: device is filled from the
  session and checked against the observer; a client cannot nominate it.
- `voice.epoch.request {call, group, epoch}`: participants whose actual
  decryptor has this epoch installed repeat their ack. Others catch up and
  ack only after installation. This never installs or activates a key.
- `voice.closed {call, participation, run, reason}` replaces the unbound
  event; `run` is null if no transport was negotiated. Defined reasons:
  `transport_failed`, `channel_restarted`, `channel_deleted`,
  `epoch_exhausted`, `stream_limit`, `state_limit`. A transport failure
  retires the run but keeps the participation for retry; the other reasons
  end it. Late answers and closures cannot affect another binding.
- `voice.slot {call, participation, run, slot, client}`: recipient's current
  binding, `slot` 0–7, source `SessionId` or null. The existing RTP
  slot-generation extension remains separate. `voice.talking {call,
  participation, run, client, talking}` instead names the **source** binding
  and source session, and keeps its existing server-wide display audience;
  outside the call it is unverified activity metadata, not a crypto input.
  For a TeamSpeak source, `run` is null. Accept only if `client` names a
  current `Client` with a channel and non-null `voice_binding`, and `call`,
  `participation` and nullable `run` exactly equal that binding. Otherwise
  discard; never use either event to install a key or choose a decryptor.

`voice.offer` is idempotent for the current run and identical fields/SDP;
return the cached answer, without replacing the peer. The same run with other
fields returns `conflict`; an older run or ended participation returns
`stale`. An SSRC already used by another run of this session returns
`conflict` with message `ssrc_reused`. A greater run retires the old stream
before negotiating its new one; failure leaves no fallback peer. Keep one
pending negotiation and one cached answer per participation, each SDP at most
64 KiB; a newer run cancels the old negotiation. Check bindings again when
async negotiation completes. A retry after a successful offer must not restart
SFrame counters. Raw UDP SSRC discovery never creates a binding.

Use existing `bad_request` for malformed fields, wrong mode/descriptor,
Add/Welcome or voice history requests; `too_large` for byte limits;
`forbidden` for failed membership/session binding; `not_found` for a missing
channel/group; `stale` for a no-longer-current call, participation, run or
epoch; `conflict` for another live group or reused run; `channel_full` for
1000 participants/leaves; `rate_limited` for the rates below; `unavailable`
for a full media command queue or the retired-stream limit. No failure may
partly admit a leaf or activate a stream. Existing authentication and the
connection's 20 requests/s bucket remain in force.

- Reuse `mls.group_info`, `mls.log` and `mls.proposal`/`mls.commit` events,
  including commit-hash reconciliation and proposal replay. Access is the
  current participation whose device is in the parent group; being a channel
  chat reader alone is insufficient. Events reach admitted and joining call
  sessions. `mls.groups` and `mls.group_history` exclude voice; discovery and
  creation settlement use `voice.state`. No voice `mls.history.*` or sealed
  chat request is accepted, even if a client guesses a group id.
- Extend the separate-tree path to `Kind::Voice`: singleton creation carries
  its tree; subsequent GroupInfos have `external_pub` and no ratchet tree.
  `GET /api/mls/{group}/tree` uses the same participation check through the
  bearer session, 30 requests/minute, no-store response, `x-gwar-epoch`, 8 MiB
  and 30 s client bounds as section 4. Retry GroupInfo and tree together if
  the epoch changed; no oversized WebSocket tree workaround.
- Existing 128 KiB decoded MLS blob and 640 KiB complete envelope limits
  apply. List/log paging is section 2's 1–100 items and byte budget, including
  pending proposals. A voice log gap forces same-group external rejoin,
  replacing the device's old leaf; it never creates a second call group.
- Store call state in the core, not SQLite: tokens/parent/group, observer and
  latest GroupInfo, leaf map, pending removals (one per leaf), log, and current
  epoch ack set. Bound each tree/serialized observer to 8 MiB, GroupInfo to
  128 KiB, occupied leaves and pending proposals to 1000, total pending
  proposal bytes to 256 KiB; log to the newest 4096 entries, 16 MiB or 90 days,
  whichever prunes first. Prune complete oldest entries. Overflow of observer
  or proposals refuses the mutation with `too_large`; admission overflow
  uses `channel_full`. Exit/removal is never refused for lack of space:
  retire routing first; if required removal state cannot fit, end the call
  with `state_limit` rather than retain a send-capable inconsistent tree.
  Calls are at most the number of nonempty channels; Gwar sessions use the
  configured server limit and bridge sessions retain their capacity rules.
  No call is allocated for an unauthenticated or rejected entry. Cache only
  the latest exported tree in the global 64-tree cache; invalidate on commit/end.
- Apply a voice commit to a copy, validate all limits and the session/parent
  checks, then atomically swap observer, leaf map, log, proposals and ack
  epoch before replying/broadcasting. Volatile voice acceptance is explicitly
  different from durable chat acceptance: process restart ends every call.
  Clear all call/transport state on server restart; reconnecting sessions
  join fresh calls. No voice group is restored from the database or allowed
  to refer to an old session. Client connection loss immediately closes its
  engine and forgets voice secrets; it never resumes a counter after reload.
- Server acks: one bit per current device at the head epoch (at most 1000),
  cleared on epoch advance, departure or run replacement. Ack requests have
  a per-participation bucket of 10/s, burst 20. Repeats have a bucket of
  10/minute per participation; coalesce call-wide repeat requests to one
  event/second. An identical ack succeeds without another broadcast except
  once in response to a new repeat round (internal round counter, not a wire
  field). No unbounded acknowledgement history or per-packet key traffic.

### SFrame and receiver state (5c/5d)

The suite is **`AES_128_GCM_SHA256_128`**, with a 16-byte authentication tag;
no shorter-tag negotiation. Application metadata is always empty. Encrypt
the entire encoded Opus packet, including its TOC; only the SFrame header
remains clear inside the RTP payload. RTP headers, slot, SSRC, timestamp and
audio level are not SFrame AAD. Use RFC 9605's header encoding, suite-specific
HKDF-SHA256 key/salt derivation, counter-to-nonce construction and authenticated
header bytes without a Gwar variant.

For each validated voice epoch E and occupied leaf L:

```text
base_key(E, L) = MLS-Exporter("gwar voice v1", u32be(L), 32)
kid(E, L)      = ((E & 0xffffffff) << 32) | L
```

The label is those ASCII bytes, without NUL or newline; the context is exactly
four binary big-endian bytes, not decimal text or base64. The group and full
epoch are already bound by MLS; do not add the channel id, call token or run
to this exporter. Feed its 32 bytes as SFrame's base key, not directly as the
AES key. Encode the unsigned 64-bit `kid` and counter with RFC 9605's variable
header, using BigInt in JavaScript; never use JS bitwise operators on Numbers.

- One encryptor per `(server, call, group, full epoch, own leaf)`, shared by
  all local sending paths. Counter starts at zero, is reserved/incremented
  before each encryption attempt, and is never reused after failure, queue
  drop, mute or activation retry. At `2^32` attempts under that base key,
  stop sending and perform an own-leaf update before another frame; never
  wrap the RFC's u64 counter. Tests still exercise the full u64 header range.
  Sending activation is monotonic in full epoch and cannot return to an old
  key; receiver reinstall cannot construct a new sender encryptor.
- Keys have one direction (RFC 9605 §4.4.1): `own_leaf` is encryption-only,
  inactive until `activate`; every other leaf is decryption-only. Own keys
  never enter receive candidates, replay windows or `last_authenticated`.
  Drop/count `own_kid_dropped` for a frame matching a retained own kid before
  trying any key, including after a leaf-index change.
- Receiver table: `BTreeMap<full_epoch, EpochKeys>` (JS ordered Map), maximum
  eight. Each entry holds `superseded_at: monotonic time or null` and
  `leaf -> {device_key, base_key, derived AEAD key/salt, Replay}`; at most
  999 other leaves per epoch. `Replay` is `{highest: u64 or none, bitmap: 128 bits}`.
  Bit 0 denotes highest; reject an already-set bit or a counter at least 128
  behind, shift/mark for a newer authenticated counter. Windows belong to the
  full `(group, epoch, leaf)` across all eight receive slots, not to an RTP
  SSRC, slot generation or current active sending epoch.
- A header's low epoch/leaf finds at most eight candidates, newest full epoch
  first. Reject malformed headers and frames outside 17–1033 bytes before
  cryptography; after opening require 1–1000 bytes of valid Opus. Try matching
  keys without changing their replay windows. Only successful authentication
  and a final atomic replay check admit a frame. A failed attempt cannot
  advance a replay window, retire keys, change a device's last epoch or reset
  a decoder. Concurrent decryptions of the same counter admit it once.
- Keep `last_authenticated[device_key] = max(full epochs authenticated)` for
  other devices still in the latest validated voice tree. The ordered
  `supersede` barrier below records E's first supersession, before key import.
  Keep E while it is the processed head, for 10 seconds from supersession,
  or while some current device's `last_authenticated` equals E. A silent
  sender's last frame thus keeps E; a sender never heard has no such entry.
  A late older frame cannot
  move this pointer back. Sweep after installation, successful receive and
  once/second; if there are over eight epochs, evict the oldest regardless
  of either retention condition. Evicted keys/windows are not reconstructed
  from saved exporters. An inactive pointer to an evicted epoch holds nothing.
- Every removal purges that device's keys/windows from **all** kept epochs,
  clears its last-authenticated entry and discards pending decrypt results
  for it before a reused leaf index is installed. A Remove plus re-add of
  the same device still purges its prior membership. Preserve the epoch
  monotonicity guard so old install messages cannot restore those keys. New membership
  gets keys only from the new validated epoch. Retention after a successful
  frame never overrides a membership removal or the eight-epoch cap.
- Per-key nonce counters and key material are never logged, persisted or
  archived. Expose aggregate counters only: `encrypted`, `decrypted`,
  `no_key`, `auth_failed`, `replay_dropped`, `own_kid_dropped`,
  `oversize_dropped`, `unexpected_duration`, `stale_install`,
  `counter_exhausted`; count a malformed frame once, not once per attempted
  epoch. Zeroize Rust keys and drop WebCrypto keys/buffers when retired/end.

### Installation and activation (5c/5d)

The worker and native engine implement the same serialized control contract.
The web MLS controller owns membership validation and the activation timer;
the decryptor owns key retention, replay windows and the sender counter.

`binding = {server_key, session, channel, call, participation, run, group}` is
fixed at encrypted engine creation, before any frame can pass. Native
`voice_start` gains that binding, `mode: "encrypted" | "plaintext"`, and
`resume_after: epoch or null`; plaintext has a null group and resume floor.
The worker's start message has the same fields. On a restart the floor is
its controller's highest processed epoch before the required own update;
refuse installs at or below it. A first creation/join has a null floor.
Direct TeamSpeak starts keep their local session/run binding, have no Gwar
call/group and reject encrypted mode. Local command `voice_set_keys` (High risk
class, never exposed to a remote web origin) and worker control messages have
this exact tagged body, with `binding` and `serial` on every variant:

| `action` | Other fields | Meaning |
| --- | --- | --- |
| `hold` | `epoch`, `removed: [device]` | stop sender, invalidate outstanding encryption work, purge these removed devices; used before processing a removal or blocking the MLS gate |
| `supersede` | `epoch` | supersession barrier for processed epoch T: record its first application time on the engine's monotonic clock for every retained/staged E < T without a supersession time; advance the processed-epoch floor before any import of T |
| `install` | `epoch`, `own_leaf`, `members: [{leaf, device, key}]`, `removed: [device]` | install the complete validated epoch: own key encryption-only and inactive, other keys decryption-only; `key` is the 32-byte exporter |
| `activate` | `epoch`, `send_until` | select the installed own-leaf sender for this epoch, if the controller's activation rule permits it; `send_until` is the earliest certificate expiry among current voice leaves, Unix milliseconds, or null if all are local identities |

Here epochs and `serial` use canonical decimal u64 **strings**, including at
the Tauri boundary (the agent already has this encoding); they are never
rounded through a JS Number. Serial starts at 1 and increases per binding;
`resume_after` uses the same epoch-string encoding; run/session follow the
wire's safe-integer bounds. `send_until` is a safe-integer Unix millisecond
timestamp or null. `members` is sorted by leaf, unique by leaf and device,
nonempty and at most 1000; `own_leaf` must
occur exactly once; `removed` is distinct and at most 1000. A control body is
at most 256 KiB. `removed` is the union of device identities removed since
the previous delivered installation, including own-device replacement; it
may also occur in `members` only as a fresh membership in this epoch.
Validate canonical keys and all bounds before enqueueing; no media key
crosses the server connection. Only the trusted local MLS controller can
call this contract, never server events directly.

- Return `{serial, epoch, result: "applied" | "unchanged" | "ignored"}`
  **after the media thread/worker actually applies it**, not when IPC posts
  it. Only completed installation permits `voice.epoch.ack`, never a barrier
  reply, `ignored` result or mere transport/worker-ready signal.
  Unknown/ended binding or lower serial is ignored before comparing keys.
  Same serial and bytewise canonical body returns `unchanged` only after
  its original application completes; different body at that serial stops
  the engine (`key_conflict`), for `supersede` too. A superseded install is
  ignored; different keys/membership at the current installed epoch stops
  the engine. Invalid structure returns `bad_keys`, without mutation.
- The processing instant is the controller's monotonic time when it marks
  T processed, after validation and any required removal `hold`. It then
  waits for `supersede(T)` to apply before publishing T or importing its keys.
  Since controller and engine clocks are separate, E's 10 s retention starts
  when the engine first applies a barrier superseding E; barrier delay can
  only lengthen retention by that delay. Retries, even with a newer serial,
  and later epochs in a pending chain preserve E's earliest supersession time;
  skipped/superseded imports cannot postpone it. Keep the processed-epoch
  floor even without installed keys; ignore older barriers and installs.
  Install requires that processed epoch; a future epoch returns `bad_keys`.
- Installs advance full epochs only; same-epoch identical keys are a no-op
  preserving sender counter and every replay window, even with a newer
  serial. Keep the latest accepted install digest, highest installed epoch
  and per-retained-epoch key identity; a lower install is ignored. A duplicate
  does not recreate a purged leaf or evicted epoch, reset a retention time,
  undo `hold`, or activate a sender. `activate` below the latest processed
  epoch is ignored. Activation of a not-yet-installed epoch returns
  `not_installed` and remains silent; retries use a new serial. Each encrypt
  checks the active gate and expiry; convert `send_until` to a monotonic
  deadline at activation and do not extend it on a clock rollback or duplicate
  install. The controller checks current membership/revocations at activation,
  on feed/parent/proposal changes and at certificate expiry, issuing `hold`
  before publishing a blocked state. A feed update never waits for an audio
  frame or another MLS commit to close the gate.
- The controller sends a `hold` barrier before marking a removal commit
  processed; the engine retires affected receive keys and old sender work
  before acknowledging that barrier. Validation may run beforehand; marking
  the commit processed after that acknowledgement is the cutoff.
  An async encrypt/decrypt completion checks binding, control generation,
  retained key identity and current removal state again before output.
  Queued output from another call is discarded. Frames encrypted before the
  cutoff are subject to the design's stated removal limit; no encryption
  after it uses the old key.
- Never await key import on the audio callback. Bound control queue to eight
  entries and fail/close the run on overflow (`key_queue_full`), without
  losing a hold or supersession barrier. Serialize control application;
  async imports must not block later barriers, and their completions recheck
  the processed-epoch floor. Superseded imports cannot publish keys or ack.
  On stop, invalidate the binding first, then drop all queued/in-flight
  results, keys and replay state.

Controller states are **Securing**, **Active(E)** and **Pending(E, target,
started, removal)**; all clocks below are monotonic:

1. Enter/create/join in **Securing**, no sending. After accepted and locally
   validated membership, mark its epoch processed, apply its supersession
   barrier, then install keys with the directions above. Record its device-key
   set, including self; only actual install completion sends `voice.epoch.ack`.
   A joiner has no preceding sender key.
2. From **Active(E)**, mark validated T processed and enter **Pending** with
   `started = processing instant` and whether a leaf was removed, then
   await `supersede(T)`. Another commit replaces T, keeps the earliest
   `started` and ORs removal across the pending chain. Import keys only after
   the barrier; neither barrier nor installation latency restarts the
   two-second wait, and import latency cannot restart supersession deadlines.
   Initial **Securing** uses the same rule with no preceding E.
3. Any removal performs the hold barrier at once; so does a blocked MLS
   gate (pending server proposal, invalid current membership or revocation).
   Until valid latest keys are installed, send nothing. A non-removal
   transition may keep using E only while the current gate remains valid.
4. Activate only the **newest processed** epoch, once its keys are installed
   and the gate is valid, if `removal`, or every device in that epoch has
   acknowledged it, or `now >= started + 2 seconds`. Include muted, deafened
   and receive-only devices and every device of one uid separately. On
   activation become **Active(T)**. Timer expiry never substitutes for keys
   or membership checks. Recheck elapsed time on every wake; suspension
   cannot start a fresh wait. A newer pending epoch never revives E after a
   removal. Initial singleton creation can activate on its own install/ack.
5. Acks are not signatures or authority: accept only the pinned call/group,
   deduplicate by `(full epoch, device_key)` and count only devices in that
   locally validated epoch. Ignore older epochs. Before processing an epoch,
   hold future acks for at most eight epochs, 1000 devices per epoch, 10 s,
   and no more than 64 epochs beyond the local head; discard oldest first.
   They cannot create keys. On catching up, request a repeat for the newest
   epoch after installation, also after losing buffered acks. Respond only
   from an installed decryptor. No ack from a removed leaf is counted for a
   different device that later uses its index.
6. If an engine/worker restarts or cannot prove continuity of its sender
   counter, enter **Securing** with a fresh run and SSRC. The retained MLS
   controller makes an own-leaf update accepted **after** this restart;
   counter exhaustion does the same, closing the old run. If MLS state was
   lost, externally rejoin the same call group instead. Until acceptance,
   refuse every older sender key and, if replay state was lost, every older
   receive key too.
   The controller releases its first install only for the epoch of that
   exact post-restart own-update/join commit (reconciled by commit hash), or
   a subsequently validated epoch. The engine's floor and monotonic install
   guard exclude older keys. Then follow normal installation/activation.
   Another device's intervening commit, or a pre-restart pending own commit,
   does not satisfy this own-update requirement.

The 2 s bound is on waiting for acknowledgements after a sender processes a
commit, not network delivery, barrier application, crypto installation, a
suspended event loop or playout. Tests distinguish those intervals. Forged
or missing relay acks can make audio switch early or at the deadline, but
cannot relax key provenance, removal cutoff or the no-plaintext rule.

### SFU, plaintext and TeamSpeak (5b)

- Replace `AudioPacket`'s assumption that every payload is inspectable Opus:
  carry opaque `payload`, explicit `Audio`/`EndOfSpeech`, clock, span estimate,
  `{channel, encrypted, call, source_session, participation, run}` and route
  generation, captured at admission. TeamSpeak packets carry their confirmed
  origin and connection/route generation instead of an RTP run. An empty or
  malformed encrypted payload is not an end marker. No encrypted payload
  reaches `media::opus::packet_samples`, decoder or TeamSpeak packetizer.
- A successful offer binds one microphone SSRC to the authenticated session,
  participation, call, run and mode. Validate the SSRC against the negotiated
  microphone section, not a first arriving packet. Each engine restart and
  call/move constructs a new transport and fresh nonzero SSRC in plaintext
  too. RTP sequence may restart only with that stream. Keep retired SSRCs
  per WebSocket session/microphone, across peer replacements: maximum 32,
  never evict to admit another. Retire before replacement; when 32 have been
  retired refuse another offer (`unavailable`, message `stream_limit`) and
  close voice with `stream_limit`. Reconnecting obtains a new session and
  DTLS transport; old UDP cannot authenticate into it.
- Before `Rtc::handle_input`, demultiplex datagrams as str0m does. For RTP
  on this microphone-only inbound transport, require a complete version-2
  fixed header and read its clear SSRC (bytes 8–11); drop unless it equals
  the negotiated microphone SSRC, before mapping/sequence allocation.
  This is a rejection filter, never authentication. Pass STUN, DTLS and SRTCP unchanged
  to str0m; RTCP feedback/report SSRCs need not be the microphone SSRC.
  Keep the post-str0m SSRC check in `ingest` before speech, slots or routing.
  Copy SSRC, str0m's **extended** sequence and raw u32 timestamp into
  `SourceClock::Rtp`; do not reconstruct ordering from arrival time or
  MediaData. Keep ordering state only for the one admitted active source.
  Drop duplicates/older sequences before routing, even after long silence;
  the TeamSpeak one-second out-of-order reset exception never applies to RTP.
- For a newer sequence, compute wrapping u32 timestamp difference against
  the last accepted timestamp of that SSRC. Difference zero drops and
  increments `rtp_zero_delta`; consume its sequence high-water mark but do
  not change the timestamp baseline or speech/slot state. Otherwise update
  source ordering even for packets subsequently gated as silence/mute.
  Positive deltas through 240000 ticks are ordinary; backward half-range or
  larger deltas increment that SSRC's source-clock generation, even when
  gated. Carry it with each admitted packet. A slot keeps its last forwarded
  generation: only within the same source/generation use the difference
  from its last forwarded source timestamp, preserving omitted time;
  a reset-sized slot delta also re-anchors. For encrypted RTP, retain the
  stream's last positive in-order ordinary timestamp step, clamped to
  960–5760 ticks, or 960 before any step. A step measures the previous
  packet plus any gap, so the 960 floor (the longest duration either client
  sends) covers configured 10/20 ms packets. A packet over 20 ms (counted
  as `unexpected_duration` at the sender) can leave the span underestimated:
  a re-anchor right after it, without a receiver reset, may drop following
  packets overlapping it, at most 4800 ticks (100 ms) per isolated event.
  Update it before gating; zero or reset-sized deltas leave it unchanged.
  Only a new run/SSRC starts afresh.
- At every re-anchor (speaker/run/SSRC change, new source-clock generation,
  reset-sized slot delta or TeamSpeak spurt/clock change), advance the slot
  clock from its last write by `max(ceil_480(elapsed), span)`, where
  `ceil_480(elapsed) = max(1, ceil(elapsed_ns / 10000000)) * 480`. The first
  packet without a prior write starts at 48000 ticks. `span` is saved with
  the slot's last forwarded packet: its parsed Opus duration for plaintext
  RTP and TeamSpeak, the source stream's estimate above for encrypted RTP.
  Preserve it across reassignment/reset and replace it only after forwarding
  the next packet; batched TeamSpeak spurt transitions use the same floor.
  An over-estimate inserts a short gap receivers conceal; an under-estimate
  makes them drop audio. Keep a u64 slot clock, write low u32; retain output
  sequence and slot-generation semantics. Forward immediately, with no
  packet-duration lookahead or new buffering. `rtp_unknown_ssrc`,
  `rtp_retired_ssrc`, `rtp_old_sequence`, `rtp_source_reset` and
  `rtp_zero_delta` are separate counters.
- **Packetization.** Keep Opus/48000 and current SDP negotiation: native
  str0m offers `minptime=10;useinbandfec=1` without `a=ptime`/`a=maxptime`;
  web passes the browser's offer unchanged. One complete encoded packet per
  RTP packet, no aggregation/fragmentation. Native Gwar keeps
  `voice::FRAME = 480` (10 ms) in both modes; browsers keep 20 ms. Direct
  TeamSpeak keeps 960 samples; the puppet's 10 ms pairing in
  [opus-10ms.md](opus-10ms.md) stays. Preserve the independent 480-sample scheduling/output quantum.
  Browsers keep capture-position timestamps. Native sends each frame's
  first captured 48 kHz sample position (plus a fixed per-run RTP offset,
  wrapping to u32), not wall time or a count of transmitted packets. Capture
  advances through silence, mute, PTT and every queue drop; discard partial
  frames across missing positions. A non-increasing faulty position drops
  and counts the frame instead of inventing media time. Preserve the current
  `CaptureSample`/`CaptureFrame` continuity checks.
- Parse Opus duration at the **sender before encryption**; count valid
  packets differing from that client's configured duration as
  `unexpected_duration`, but send them. Drop malformed Opus and encoded packets
  over 1000 bytes, never truncate. SFrame adds at most 33 bytes; cap incoming
  encrypted payload at 1033 before work/allocation. Plaintext RTP also has
  the 1000-byte encoded cap. Receivers decrypt before duration inspection,
  jitter insertion, decoder reset or playback. The SFU never needs a frame's
  encrypted duration. Keep RFC 6464 audio-level speech gating unchanged.
- **Plaintext** channels remain unencrypted and, with compatibility enabled,
  bridged. They get call/run fencing, fresh SSRCs and capture timestamps
  with unchanged packetization; the worker/native pipeline selects plaintext
  mode at start, without trying SFrame then falling back. Existing
  direct TeamSpeak transport remains plaintext with its u16 packet-id and
  talk-spurt clock. Preserve its variable-duration packetizer and ordering;
  all SFU re-anchors use the 10 ms rounding and previous-span floor above.
- **Bridge boundaries.** `Member.bridged` remains an early gate but is not
  enough for a queued packet. SFU fan-out, `Forward`/`ForwardTo`, sinks,
  puppet uplink queue, puppet socket and incoming voice/whisper each check
  the packet's fixed origin/mode and the current route generation. Require
  plaintext origin, a still-current source participation/route and a
  confirmed matching TeamSpeak channel before sending. Capture origin at the
  TeamSpeak receive callback, not later from mutable `Remote.cid`. Recheck
  target mode for whispers. Pending/refused moves, reconnect, a retained
  direct-message puppet and grace periods cannot bypass the checks.
  Increment route generation on every move attempt and transport replacement;
  drop queued audio from the previous route, including plaintext across moves.
  Explicit local end-of-speech control may clear old talking state, but sends
  no payload into a new channel. Never relabel queued packets with the
  sender's later channel. Keep section 4's analogous text boundaries.

### Web and desktop (5c/5d)

- `apps/web/src/voice/webrtc-engine.ts` creates one dedicated SFrame worker
  per encrypted run. Attach `RTCRtpScriptTransform` to the mic sender and all
  `AUDIO_SLOTS` receivers immediately when creating their transceivers,
  before attaching a live track or setting descriptions. Create the mic
  transceiver without a track, attach its transform, then `replaceTrack`.
  Worker starts closed, drops without keys, encrypts/decrypts one frame at a
  time per transform and never passes plaintext through an encrypted path.
  Share receiver key/replay state across slots. Async output uses the
  installation/removal fences above. Keep at most one in-flight and one
  waiting frame per transform; drop excess rather than add a latency queue.
- Probe `RTCRtpScriptTransform`, worker transform events and WebCrypto in the
  actual engine. A failed probe or worker failure prevents encrypted voice;
  it does not change the channel's mode. Unsupported browsers show
  "Update your browser or use the desktop app to join encrypted voice."
  Check before microphone permission or `channel.join`. Browser settings
  and device changes that recreate the engine follow the own-update rule.
- Extend `apps/web/src/mls/runtime.ts` with the volatile voice controller and
  cancellation under its device lease. All tree downloads, commits, timers,
  installs, promises and acknowledgements capture the full binding. A move
  synchronously closes the old media gate and discards captured, encoded,
  queued and decoded old-call audio before starting the new run. A Web Lock
  takeover closes voice before releasing the old tab's lease. Post key
  material only on installation, never per frame or in React state/logs.
- `apps/web/src/voice/native-engine.ts` relays the same contract to
  `apps/desktop/src-tauri/src/voice.rs`; Rust `vc-client::voice` is the actual
  encryptor/decryptor. Bind `voice_start`, `voice_answer`, `voice_stop`, key
  commands and asynchronous events to the current run; stale stop/answer
  cannot cancel a new run. Validate in both Tauri and the engine; return
  `voice_set_keys` completion from a media-thread acknowledgement. Attach
  crypto before starting the RTC send/receive loops. Gate encoded sending
  immediately before encryption/RTP write and decrypt before producing an
  `Incoming` Opus packet. Worker status and native `voice://crypto` emit
  `{binding, state: "securing" | "failed", reason}`; reasons are
  `counter_exhausted`, `key_expired`, `key_conflict`, `key_queue_full` or
  `worker_failed` (web only). Exhaustion/expiry closes sending and wakes MLS
  reconciliation (including rejoin for an expired own leaf); a failed state
  closes the run. No status event counts as
  an installation acknowledgement. Keep native input/output diagnostics.
- Rust uses the **crates.io `sframe` 2.1.0 release**, which contains the RFC
  9605 AAD-order fix (upstream issue
  [#124](https://github.com/TobTheRock/sframe-rs/issues/124), merged PR
  [#125](https://github.com/TobTheRock/sframe-rs/pull/125)); an unpatched
  2.0.0 is not acceptable. The shared metadata vector proves the order.
  Use explicit counter/replay ownership around the library; do not rely on
  a constructor that silently resets state on reinstallation.
- Native playout accepts every valid Opus packet duration from 2.5 through
  120 ms (120–5760 samples), within the encoded-size cap. Use the actual
  decoded length, retain unused PCM across output callbacks, and use the
  packet's real duration for jitter/PLC accounting. Never pad/truncate a
  decoded packet to 960 samples. Reset decoder, packet queue and PCM tail
  for a **validated** new source/slot generation; preserve the existing
  variable-duration tests and extend them through SFrame.
- **Interface.** Until a sending key is active, show "Securing voice…" in
  the voice status, in the existing muted style; receiving may already work.
  Use "Voice unavailable" and "Retry" for a stopped engine, no alarm banner
  or red encryption warning. Channel info shows "End-to-end encrypted" and
  a separate "Who can hear" list from the locally validated voice tree,
  grouped by uid with each device visible; do not derive it from slots,
  talking flags or relay acks. Pending removal remains listed until the MLS
  commit removes it. A client outside the call says "Join voice to see who
  can hear" instead of presenting server metadata as a verified roster.
  Plaintext channels keep "Not end-to-end encrypted". Chat's "Who can read"
  stays a separate list. Strings go through the existing i18n mechanism.

### Tests and acceptance

Each PR runs its affected unit/integration suites plus the existing plaintext
web and TeamSpeak regressions. Test results name the commit, platform and
what actually ran; compilation, fakes and simulation are not live browser,
physical audio or official-server acceptance.

- **5a, `gwar-mls`/protocol:** descriptor and parent bindings; wrong creator,
  duplicate device, forbidden Add/Welcome/history; no export while Creating
  or Joining; lost create/commit reply, competing creates/commits, stale
  participation; parent removals vs historical catch-up and a newcomer's
  admission ahead of parent sync, initially and mid-call; safe-integer wire
  rejection and full-epoch key ids. Agent and observer agree on every tree
  and staged transition. Verify voice never appears in durable chat storage,
  key archives, history requests or the channel auto-join loop.
- **5a, server integration:** two users with two devices each; create, join,
  leave, rejoin, disconnect, same-device takeover, own-leaf replacement,
  last leave, new call, deleted/restarted parent, server restart mid-call.
  Cover takeover at capacity/rejected authentication, bridge-only and mixed
  calls, bridge/forced moves, and roster-before-talking ordering with stale
  participation/run events. Assert mapping, proposals on every exit, first-wins group
  settlement and immediate routing cutoff. Inject duplicate/future/stale
  acks, forged device fields, lost repeat requests, expired pending state,
  offer completion after move and old-run close/stop. Exercise every cap,
  log/tree paging and a pruned participant's same-group rejoin; plaintext
  remains usable throughout. Rejected operations publish no partial state.
- **5b, SFU units/integration:** arbitrary non-Opus ciphertext passes byte for
  byte; header inspection cannot drop it. Unknown, retired and reused SSRC,
  retirement saturation, sequence and timestamp wraps, duplicates,
  reordering, zero-difference counter, backward/reset and >5 s silence,
  mute/PTT gaps, bursts, source/slot changes, late old-call packets before
  and after routing. Unknown-SSRC churn with valid SRTP and negotiated MID
  must not grow transport state or disturb the admitted stream; RTCP still
  works. Verify 480-tick rounding with the previous-span floor: forward a
  20 ms browser packet at 48000, then reset/change source within 10 ms; the
  next timestamp must be at least 48960, never 48480. Cover 10 ms native
  sources (learned 480 clamped to 960, initial 960, and a 10 ms packet
  followed by a 20 ms one, then a reset within 10 ms), 60 ms TeamSpeak packets
  batched across spurt changes (floor 2880), and every re-anchor cause.
  Assert each new anchor is outside the previous packet's span for
  configured encrypted durations and parsed plaintext/TeamSpeak durations,
  including zero elapsed time and switching 10/20 ms speakers; an encrypted
  120 ms packet followed by a reset documents the bounded exception.
  Separately check estimate clamping and that zero/reset/old packets
  cannot change it.
  Through native decoded playout, reset the same source after a 20 ms
  packet with unchanged slot generation: require the post-reset tone in
  output and no late drop, not just increasing RTP timestamps. In 5b use
  plaintext Opus with the encrypted-clock estimator; repeat through SFrame
  in 5d. With increasing sequences and timestamps
  `100000 -> 100960 -> 100000 -> 100960`, gate the middle packets: forwarding
  again after 60 ms advances 2880, not 960 ticks.
  Require monotonic output and no extra buffer in plaintext/encrypted modes,
  plus TeamSpeak's independent packet-id/spurt clock and unchanged pairing.
- **5b, official TeamSpeak in CI:** extend the existing
  `cargo test -p vc-server --test teamspeak_channels -- --ignored --test-threads=1`
  job using its checksum-verified official server and `VC_TS3_DIR`. The
  current permissions/locked-channel check is retained; add S6 audio/text
  probes with a working plaintext control for **each** route: moves during
  speech, delayed packets from before classification, grace-period and
  direct-message puppets, refused moves, voice/whispers both directions,
  queued packets/text across reconnect and query/listener routes. Count zero
  encrypted-channel payload bytes at the TeamSpeak egress and zero admitted
  TeamSpeak payloads in encrypted voice; channel permissions alone do not
  pass this test. Use real SFrame endpoints again in 5c/5d. Do not mark the
  design's official-server S6 measurement complete merely because the job
  exists or fake boundary tests pass.
- **5c/5d, shared crypto and state-machine fixtures:** RFC 9605 header,
  KDF/AEAD and ciphertext vectors, including **non-empty metadata** to prove
  the upstream AAD-order fix in the low-level primitive, are mandatory CI
  checks for both Rust and WebCrypto. Production accepts empty metadata only.
  Cross-decrypt Rust/browser frames at short and widest kid/counter encodings,
  and 1000-byte packets plus full overhead through the real SFU. Tamper every
  header/ciphertext/tag field, inject plaintext, unknown keys and replays
  across slots; no plaintext output or replay state mutation on failed auth.
  Test overlapping async decrypts, 128-packet window boundaries, epoch
  low-half wrap/collision, leaf reuse, own-kid rejection without decrypt or
  receive-state mutation, silent sender retention, 10 s expiry from first
  engine barrier application despite delayed/skipped imports and retries,
  ninth-epoch eviction, old frames after newer authentication, key conflict,
  stale/duplicate installs, certificate expiry during uninterrupted speech,
  revocation while encryption is in flight, clock rollback and lost install
  acknowledgements. Count zero repeated `(base key, counter)` across all
  transitions and restarts. With independent controller/engine clock origins
  and a missing ack, process a non-removal at test time 0: delay only its
  barrier to 1 s and install promptly; activation is at 2 s and E's
  retention deadline at 11 s. Separately apply the barrier at 0 and delay
  import to 3 s: activate on installation, with E's deadline still at 10 s.
  Use E with no last-authenticated retention and fewer than eight epochs;
  sweep at the deadline. Retry barriers/installs and replace pending targets
  without moving either original deadline.
- **5c, browser e2e in Chromium and WebKit:** two users, two devices per user,
  real server and synthetic mic tones observed at the receiving WebAudio
  output. Join/leave/rejoin, a newcomer, eight speakers/slot reuse, lost and
  duplicate acks, overlap of adds and removals, worker/engine restart, device
  switch, server restart, missing keys, background tab and plaintext control.
  Timestamp sender commit processing, decryptor installation, activation,
  first authenticated new-epoch packet and first decoded tone separately.
  With commits delivered and keys installed, activation waits at most 2 s
  (test-clock assertion); audible newcomer delay is bounded by that wait
  plus the measured unchanged transport/playout delay. With prompt acks it
  must not wait for the timer. Keep the removed device's keys in the test:
  it opens no frame **encrypted after** each sender's removal cutoff, even
  when the SFU deliberately delivers one. Withheld commits and stalled
  installation stay non-send-capable when no valid sender remains, with
  Securing shown instead of a misleading 2 s delivery guarantee.
  A missing browser engine/transform is an unrun acceptance check, not a pass.
- **5d, native:** actual media-thread key-install completion; delayed,
  duplicate and reordered IPC; retired run, post-stop command, conflict,
  same-epoch counter/window preservation and reset requiring own update;
  capture position through mute/PTT/ring overflow/outgoing drops; native
  10 ms packets in both modes, unchanged SDP and direct TeamSpeak's 20 ms.
  Decrypt/decode every valid Opus duration, mix fractional tails, packet loss/PLC, and reset a slot
  only after authentication. Native-to-native and native-to-Chromium/WebKit
  tones both ways must reach decoded playout, with all eight slots exercised.
  Extend `apps/web/e2e/native-audio.mjs` beyond its current plaintext
  Chromium case. Physical microphone/speaker acceptance is recorded
  separately from virtual-I/O tests.

**Latency:** encryption adds no measurable buffering or packetization delay.
Use [latency.md](latency.md)'s native `latency` example, separate target
folders and `scripts/latency/run_local.py` alternating baseline/candidate
runs (`--markers 100 --runs 4`, at least 400 markers per variant). Gate 5b's
timestamp/bridge changes against the pinned pre-step baseline commit, both
at native's real 10 ms packetization. In 5d extend the harness to join a real
voice MLS group and use production key installation, then compare plaintext
and SFrame at the same native 10 ms, bitrate, callbacks and jitter settings.
Neither comparison changes packetization. Do not change playout tuning in
the SFrame PR.

Record median/p95/p99, marker matches/loss, concealments, active underruns,
queue/PCM-tail sizes and worker/crypto time; keep raw CSV/JSON, versions,
hardware and load conditions. Warm up as the tool does, use balanced order,
and run no concurrent builds/tests. Repeat with its documented loss,
reordering, burst and callback-jitter profiles, then controlled CPU load.
Require at least 95% matched markers, no below-floor/aliased match, and an
added mouth-to-ear p95 of at most 2 ms in each of the 5b and 5d comparisons;
a noisy run that cannot resolve 2 ms is inconclusive, never a pass. Also
require no added sustained queue, unexplained marker loss or active underruns.

Measure browser/native interop with the same marker identity method at real
WebAudio/native decoded output; worker timings alone cannot see upstream and
downstream browser queues. The S2 gate is added transform time including
observable queueing p99 below 2 ms, foreground and genuinely backgrounded
Chromium/WebKit under normal browser policy, plus end-to-end comparison.
Report unavailable timestamps explicitly and use end-to-end measurements to
cover them; minimized/covered-window results do not stand in for hidden tabs.
Remote VM S4 and physical-device measurements remain separate acceptance
runs under the owner's environment authorization; this documentation task
connects to no remote hosts and claims none of those measurements.

**Step 8:** independent review of the WebCrypto SFrame worker (RFC encoding,
key derivation, counter uniqueness, replay/retention, asynchronous control and
fail-closed boundaries), and the existing OpenMLS provider/storage reviews.
Check the native wrapper/IPC binding and cross-implementation vectors as
part of that review. Native SFrame already uses the fixed crates.io release
(sframe 2.1.0); a green interop test does not substitute for the worker
review.

## 6. Private channels

Status: **specified, not built**. A private channel is hidden from everyone
it does not admit. It can be plaintext or encrypted, chosen at creation as
today:

- In a **plaintext private channel** the server alone decides who is
  admitted, from roles and listed members (6a). This is the server's word,
  like everything else in a plaintext channel.
- In an **encrypted private channel** the admitted members are a list signed
  by the server's owner or a manager and kept in the channel's MLS group, so
  a dishonest server cannot add a reader (6c). Its authority comes from the
  owner chain and the manager list (6b).

Three pull requests, in this order:

- **6a:** `private` and `access` on channels, hiding, entry and moves, role
  grants, plaintext chat and files, TeamSpeak (not mirrored), channel
  tombstones in clients, and the web interface. Encrypted private channels
  are refused until 6c.
- **6b:** the server owner, the owner chain, the confirmer anchor and the
  manager list with their checkpoints, pending managers, and the web
  interface.
- **6c:** the access extension `0xff01`, the agent's and observer's rules, the
  server's read rule from the group, joining, list changes, offers and
  re-signing, restarts, voice under a private parent, and the web interface.

### Private and access (6a)

- `Channel` gains `private: bool`, fixed at creation (`channel.create` takes
  it; `channel.update` cannot change it).
- A private channel has an **access** set:
  `access: {roles: [group id], users: [uid]}`, at most 64 roles and 1000
  users, sorted and unique.
  - `channel.create` takes it, defaulting to the creator.
  - `channel.update` replaces it, which needs `channel_edit` and admission.
  - Only admitted sessions receive it.
  - Listing a role admits the members who hold it at that moment.
  - Deleting a role removes it from every access set.
- **Admitted** sessions:
  - for a plaintext private channel: holders of the Admin group, members of
    a listed role, and listed uids. The server owner is admitted from 6b on.
  - for an encrypted private channel (6c): the uids the channel's group
    admits. Its `access` is read-only, `users` reflects the current signed
    list, and `roles` is empty.
  - TeamSpeak users are never admitted.
- A private channel has **no password** (`bad_request`), **no children**
  (`channel.create` and `channel.update` refuse a private parent) and is
  **never the default channel** (`server.update` refuses it). It may itself
  sit under a public channel.
- **Granting a role is delegation.** A role, and with it every private
  channel it admits, is given out only by someone who **holds that role or
  the Admin group**, on top of the existing permission check (the role's
  permissions are a subset of the granter's). The rule is the same for every
  role, so whether a grant succeeds never depends on hidden channels, and
  only an Admin grants Admin (which admits every plaintext private channel).
  It applies to:
  - `member.groups` (both the roles added and the roles removed);
  - `token.create` and invite creation, and again at redemption, as
    `could_grant` already does for permissions;
  - any other role assignment.

  Adding a role to an access set likewise requires the editor to hold that
  role or the Admin group.
- **Hiding.** For a session that is not admitted, the channel does not exist:
  - `Welcome.channels` omits it, and channel events are not sent;
  - a client inside it is reported with `channel: null`, `voice_binding:
    null`, and `talking` and `voice` false (on the server, not in voice);
  - `voice.talking`, `voice.slot` and every other event about its occupants'
    voice reach only admitted sessions;
  - chat, edit, delete and read-mark events for it reach only admitted
    sessions;
  - read marks, unread counts, mentions, `Welcome.conversations` and
    `mls.groups` leave it out;
  - a request naming it, directly or through a message id, answers
    `not_found`, as for a deleted channel;
  - **sibling names** are unique only among the channels the requester can
    see: creating or renaming a channel never fails because of a hidden
    sibling, so two siblings may share a name when one is hidden from the
    other's creator.
- **Admission changes.** On every change (access edited, role granted or
  removed, role deleted, Admin group changed), the server compares each
  session's admissions before and after:
  - **A session that lost admission:**
    - it leaves the channel if inside (it stays on the server, not in voice,
      and its voice participation closes as on a move);
    - it receives `channel.hidden {channel}`;
    - it then receives `client.updated` for every occupant, now masked.
  - **A session that gained admission** receives `channel.created` with the
    access, then `client.updated` for every occupant, unmasked.

  `channel.hidden` is distinct from `channel.deleted`: the channel still
  exists and keeps its identity.
- **Channel tombstones in clients.** Clients keep the existing channel
  lifecycle record (a reused id qualifies its messages) and separate:
  - **deleted:** seen in `channel.deleted`. The id must never return.
  - **hidden:** seen in `channel.hidden`, or missing from a Welcome. It may
    return as the same channel.

  A Welcome cannot tell a deletion that happened while the client was away
  from a hiding, so such a deletion is no longer detected. Explicit
  `channel.deleted` events still are. Hiding drops displayed content and
  plaintext caches. For an encrypted channel (6c) it never drops the
  group's anti-rollback state: the agent keeps a tombstone pin (conversation,
  incarnation, checkpoint, access checkpoints) for every group it forgets,
  for deleted channels too.
- **Entry and moves.**
  - `channel.join` into a private channel requires admission
    (`not_found` otherwise).
  - A moderator move requires the moved client's admission (`bad_request`
    for an admitted mover).
  - `ChannelJoinLocked` does not bypass admission.
- **Plaintext chat and files** follow admission:
  - `can_read` requires it, which covers history and read marks.
  - Attachments of messages in a private channel need an authenticated,
    admitted download in **both** file access modes. In Link mode a bare link
    to them returns `not_found`.
- **TeamSpeak.** The bridge never learns of a private channel:
  - no channel note, snapshot entry, event or text;
  - a Gwar user inside one appears to the bridge as not in voice, so it has
    no puppet while there.
- **Interface:**
  - Create dialog: "Private" (Gwar servers only), with "Who can see this
    channel": roles and members, defaulting to the creator. Encryption is
    hidden while "Private" is on until 6c.
  - Channel settings: the same access picker for `channel_edit` holders.
  - Private channels show a small closed-eye icon.

### Owner chain (6b)

- **Initial state.** A server starts **without an owner**:
  - `Welcome.owner` is `null`;
  - clients pin nothing for the owner;
  - 6b features are inactive (no manager list, no encrypted private
    channels).
- **The first owner.**
  - The first admin token is marked durably when issued (`tokens.first`).
  - Redeeming it makes the redeemer's uid the owner, in one transaction with
    the Admin grant and the genesis (meta `owner_generation = 1`, the genesis
    below), **only while the server still has no owner**, checked inside that
    transaction. Once there is an owner (`owner set`, or an earlier
    redemption), the first token grants Admin only and never writes a
    genesis or a generation.
  - Other admin tokens never make an owner.
- **Operator commands** run locally on the database, like the other
  `vc-server` admin commands, with the server stopped:
  - `vc-server owner set {uid}` sets the genesis only while there is no
    owner, for a uid that is a stored member;
  - `vc-server owner reset {uid}` (lost owner account) starts a new
    **generation**: it writes a new genesis with the generation incremented,
    and drops the chain, the confirmer anchor and the manager list of the old
    generation.
- **Genesis** is the server's word, pinned on first contact as the server key
  is. All statements in this section are UTF-8, lines separated by a single
  `\n`, with no trailing newline. Decimals are canonical (no leading zeros,
  at most 2^53 − 1). Uids and keys are canonical unpadded base64url. Hashes
  are SHA-256 of the statement bytes alone, encoded as base64url.

  ```text
  gwar owner genesis v1
  {server_key}
  {generation}            decimal, 1 first
  {owner_uid}
  ```

  `H0 = SHA-256(genesis)` is link 0. **Ownership moves** by links, each
  signed by the previous owner's **account key** (raw Ed25519 over the
  statement):

  ```text
  gwar owner v1
  {server_key}
  {generation}
  {seq}                   decimal, 1 for the first move
  {previous_link_hash}    hash of the previous link's statement (H0 for seq 1)
  {previous_owner_account_key}
  {new_owner_uid}
  {anchor_hash}           hash of the confirmer anchor handed over, or -
  {signed_at}             Unix ms
  ```

  - The link is valid when `uid_for_key(previous_owner_account_key)` is the
    previous owner's uid and the signature verifies.
  - The head is then `(generation, seq, hash)`.
  - Account keys sign only with the password (Connect) or the local identity.
- **Protocol:**
  - `Welcome.owner = {uid, generation, seq, head}` or `null`.
  - `owner.chain {generation, after}` returns the genesis and the links after
    `after`, at most 64 per page with `next`.
  - `owner.transfer {statement, signature}` checks the link and, in one
    transaction, that its previous link hash is the current head **and** its
    `anchor_hash` is the hash of the server's current anchor (`-` only while
    there is none); otherwise it answers `conflict` and the owner's client
    re-signs. It then appends the link and gives the new owner the Admin
    group (the old owner keeps their roles), and broadcasts `owner.updated`.
- **Client pins**, per server key: `(generation, seq, head)` and the owner
  uid.
  - **With a pin:** on connect, the client fetches the chain of its
    generation after its seq and accepts only an extension of the pin. A
    different chain, or a lower seq, disconnects with "The server's owner
    record changed".
  - **Without a pin:** the client takes the server's genesis and chain (trust
    on first use), or the owner named by an invite link (`owner={uid}`).
  - **A higher generation** (an owner reset) is the neutral notice of a
    changed owner, with explicit acceptance. Accepting re-pins and clears
    every 6b checkpoint of the old generation in one write; private channels
    then need lists of the new generation (6c).

### Managers (6b)

The managers of encrypted private channels come from two statements:

- **The confirmer anchor**, signed by the owner's **account key**. It names
  the confirmers, who may maintain the manager list. It changes rarely and
  never expires.
- **The manager list**, signed by a **device** of the owner or of a confirmer
  on the anchor. It lists plain managers and repeats the anchor's
  confirmers.

```text
gwar confirmers v1
{server_key}
{generation}
{owner_head}             hash of the owner chain head it was signed under
{anchor_seq}             decimal, 1 first
{confirmer uids}         comma-separated, sorted, unique; - when empty
{signed_at}
```

```text
gwar managers v1
{server_key}
{generation}
{anchor_hash}            hash of the confirmer anchor it is signed under
{seq}                    decimal, 1 first, across anchors
{manager uids}           comma-separated, sorted, unique; - when empty (confirmers not repeated)
{signer_uid}
{signed_at}
```

- **Envelopes.**
  - The anchor is three netstrings: the statement, the signing owner's
    **account public key** (base64url), and that key's raw signature
    (base64url). Neither the genesis nor a link carries the current owner's
    key, so the anchor must.
  - The list is three netstrings: the statement, the signer's credential, and
    its device's raw Ed25519 signature.
- **Bounds:** at most 32 confirmers and 100 managers, never including the
  owner, and a manager is never also a confirmer. An encoded anchor plus list
  is at most 16 KiB.
- **An anchor is valid** (as history) when `owner_head` is a link of the
  reader's pinned chain of that generation, `uid_for_key(key)` is the owner
  at that link (the genesis owner, or the `new_owner_uid` of that link), and
  the signature verifies. Superseded anchors stay valid in this sense, so
  past commits and lists still verify after anchors rotate and ownership
  moves.
- **An anchor is current** (may be accepted at a checkpoint and confer
  authority) when it is valid and also:
  - **new:** `owner_head` is the head of the reader's pinned chain (after
    fetching any extension of the pin): only the current owner mints
    anchors; or
  - **inherited:** ownership moves hand over one anchor by hash. The owner
    link has a line `{anchor_hash}` (the server's anchor when the link is
    accepted, see `owner.transfer`, or `-`). An anchor naming an earlier
    head is current only if every link after that head names its hash.
  - A former owner can therefore never sign an anchor that a reader of the
    newer chain accepts as current, whatever its `anchor_seq`.
- **Validity of a list** (as history):
  - the anchor it names is valid;
  - its signer is a confirmer on that anchor, or an owner of the anchor's
    span: the owner at its `owner_head`, or at any later link while every
    link up to it names the anchor's hash;
  - the credential is well-formed with uid `signer_uid`, and the signature
    verifies.
- **A list may be accepted as current** only when, in addition, its anchor
  is current and its signer is the **current owner** (at the reader's pinned
  head) or a confirmer on that anchor. A new owner therefore renews an
  inherited anchor's list, and a former owner's list signed after the move
  is history at most.

  Every list is self-contained with its anchor, so a reader that missed any
  number of updates verifies the newest pair without history.
- **Current**: a list is current while its signer's device certificate is
  current (not expired, not revoked) now. The anchor never expires.
  - Authority comes only from the newest accepted list, and only while it is
    current.
  - A reader accepts a newer list only if it is current when the reader first
    sees it and its anchor is current (above).
- **Checkpoints**, per server key and generation:
  - `(anchor_seq, hash)` and `(seq, hash)`;
  - a lower seq is rejected, and the same seq is accepted only with the same
    hash;
  - the agent's server record `{server}/@server` keeps them with the owner
    pin, written atomically.
- **Mirroring `channel_edit`.** The server reports the following in
  `Welcome.managers = {anchor, list, pending, stale}` and in
  `managers.updated`:
  - `pending`: holders of `channel_edit` other than the owner and the
    confirmers who are not on the list;
  - `stale`: plain managers who lost `channel_edit`, and confirmers who lost
    it.
- **Who signs:**
  - **The clients of the owner and of current confirmers:**
    - remove stale plain managers automatically;
    - add a pending uid automatically when their own role change granted
      it, otherwise after "Allow @name to manage private channels?".
  - **The owner's client:**
    - changes confirmers by signing a new anchor (asks for the password),
      also for stale confirmers;
    - then signs a list under it, the same seq + 1.
  - **Plain managers** sign nothing.
- **Renewal.** A list is re-signed unchanged (seq + 1) when it is older than
  60 days, or when its signer's certificate expires within 7 days or was
  revoked. Any current confirmer or the owner does this; authority to renew
  comes from the anchor, which does not expire. If no owner or confirmer
  device is current, newcomers cannot join encrypted private channels (6c)
  until one is.
- **Protocol:**
  - `managers.update {anchor, list}`: the server checks validity, seqs,
    that the anchor and list are current under **its current owner head**
    (as a reader would accept them), and the signer's current authority, in
    the same transaction that stores the pair; `owner.transfer` and
    `managers.update` are serialized, so neither order installs a pair
    readers of the new chain would refuse. It then broadcasts
    `managers.updated`.
  - The server keeps the newest anchor and list of the current generation.

### Encrypted private channel groups (6c)

- **Descriptor:** `private = 1`, `founding_owner` = the owner uid at creation
  (history only). The creator must be the owner, a confirmer or a manager on
  the newest current list the creator accepted. The server checks the same
  against its newest pair.
- **The access extension `0xff01`** in the GroupContext holds three
  netstrings:
  1. the confirmer anchor;
  2. the manager list (both as above);
  3. the **member list**: three netstrings (statement, signer credential,
     signature) of

     ```text
     gwar channel members v1
     {server_key}
     {generation}
     {group_id}              base64url: the list is bound to this group
     {seq}                   decimal, 1 first in each group
     {member uids}           comma-separated, sorted, unique; - when empty
     {signer_uid}
     {signed_at}
     ```

  Bounds: at most 1000 members. The extension is at most 48 KiB, which
  leaves room under the 128 KiB commit and GroupInfo limits.
- **Signer of the member list:** the owner, a confirmer or a manager on the
  extension's own manager list. It does not name a manager list: as long as
  its signer stays on the manager list, a new manager list keeps it valid.
- **Capabilities.** From 6c on every key package and leaf advertises `0xff01`
  as well as `0xff00`. A private channel's group requires both in its
  RequiredCapabilities, set at creation. Other groups require only
  `0xff00`, as today. Old key packages are replaced, as this is the dev
  stage.
- **Membership rule** (the agent's and the observer's tree rule): every
  leaf's uid is the owner (of the extension's generation), a confirmer, a
  manager, or a member on the extension's lists.
- **Changes.** Any member may commit a `GroupContextExtensions` proposal that
  replaces `0xff01` only. `0xff00`, the external senders and the required
  capabilities stay byte-identical: the proposal carries the whole set, and
  the agent copies the others.
  - **Validation as history** (the agent and the observer, for every commit,
    including replay):
    - the new extension parses, and every signature verifies (its anchor
      valid as history; it need not be current);
    - the anchor's and the list's seqs are at least the previous ones', and
      the member list's seq is greater, or equal with the same bytes;
    - the member list's signer is authorized by the new manager list;
    - every signer's certificate covers the commit's time.
  - **The same commit** removes every leaf the new lists no longer admit.
  - The allowlist gains `GroupContextExtensions` for private channel groups
    only, never in an external commit.
- **Current authority, separately** ("Removal" in [e2e.md](e2e.md)):
  - Replaying history never lowers a checkpoint and never makes a list
    authoritative.
  - A device sends, shares history and joins voice in a private group only
    while all of these hold:
    - **(a)** the group's manager pair is the newest pair this device
      accepted for the server (server-wide checkpoint);
    - **(b)** the group's member list is the newest it accepted for that
      group;
    - **(c)** both were current when it first saw them;
    - **(d)** the tree holds no leaf they exclude.
  - Otherwise it blocks sending and repairs what it can:
    - it commits the newer manager pair into the group with the removals it
      requires, when its member list's signer is still authorized;
    - it commits the removals for (d);
    - otherwise it waits for a manager.
- **Who commits:**
  - The owner, confirmers and managers are always admitted. Their devices
    join every private group, which lets them change it.
  - Editing access in the interface signs a new member list (roles are
    expanded to their current members at signing time) and commits it with
    the removals.
  - When a newer manager pair is accepted, every member carries it into its
    private groups as above (one group at a time, random delay up to 10 s, so
    concurrent carriers settle on the first commit).
  - A stale commit is recomputed from the new epoch. A losing list edit
    needs a new signature at the next seq; the old signed list is never
    retried.
- **Offers.** A manager who cannot commit to a group, because it is not a
  member and the group's lists are no longer current, sends
  `mls.access.offer {group, extension}` with a fresh signed extension.
  - The server checks the conversation and the signer's authority against
    its newest pair, and forwards it as `mls.access.offered` to the group's
    readers.
  - Any member that validates it (as a commit would) commits it. The offer
    itself grants nothing.
  - If no member can commit any more, the owner or a confirmer restarts the
    channel (below).
- **Newcomers:**
  - A device joins only if (a)–(c) hold for the lists in the current
    GroupInfo, as of now. Otherwise it shows "Waiting for a channel manager
    to confirm access" and retries after `managers.updated`,
    `mls.access.offered` or every hour.
  - The owner's, confirmers' and managers' clients re-sign a member list (seq
    + 1, same members) when it is older than 60 days or its signer's
    certificate expires within 7 days or was revoked.
- **Checkpoints**, per group in the agent's group record: `(seq, hash)` of the
  newest accepted member list. They are kept in the tombstone pin when the
  group is forgotten, so a channel that is hidden and shown again can never
  roll its list back.
- **Server:**
  - the observer parses `0xff01` after every commit;
  - `mls_can_read` for a private channel's conversation is the extension's
    admitted set;
  - channel visibility (6a) follows the same set;
  - `mls_missing_proposals` proposes removing leaves the current set does
    not admit (with the existing reasons: not a member, banned, revoked);
  - `mls.group.create` for a private channel checks the creator against the
    server's newest manager pair.
- **Restarts.**
  - Only the owner, a confirmer or a manager may create a successor of a
    private channel's group (the descriptor creator check).
  - The successor's initial `0xff01` carries the newest manager pair and a
    member list **bound to the successor's group id, seq 1**, with the
    predecessor's members unless edited.
  - Member-list checkpoints are per group: the successor's do not compare
    with the predecessor's. The incarnation pins (section 4, "Restarts")
    prevent rollback to an earlier group.
  - **Only the final successor carries authority.** Catching up along the
    chain checks intermediate incarnations exactly as section 4 does
    (descriptors, signatures, certified creators), with no manager check:
    their creators may have lost authority since, and nothing about them is
    used. The chain is fixed by the signed `previous_group_id` links, so a
    fabricated intermediate is reachable only from a final successor that
    names it.
  - **Staged adoption.** For a private conversation `accept_chain` verifies
    the chain but changes nothing until the final successor passes: its
    current GroupInfo satisfies (a)–(c) as of now, and its creator is the
    owner, a confirmer or a manager on the reader's current pair. Only then
    are the pins, the live incarnation and the superseding written, in one
    write. Otherwise the predecessor's state, outbox and pins stay as they
    were, and the client shows "Waiting for a channel manager to confirm
    access" and retries as a newcomer does.
- **Voice.**
  - A private channel's voice group binds to its parent as in 5a. The agent
    no longer refuses a private parent.
  - Voice membership is the parent's current tree. Joining, **and every
    voice key export and sender activation for the whole call**, need the
    parent's current-authority conditions (a)–(d) above.
  - Accepting a newer manager pair or member list that the parent does not
    yet carry is a blocked MLS gate: the agent issues the `hold` barrier at
    once (section 5d) and transmits nothing until the parent carries the
    accepted lists and its tree holds no excluded leaf.
- **History** is unchanged: it is shared among the group's members.
- **What the server can still do**, recorded in [e2e.md](e2e.md)'s first-contact
  and delivery limits:
  - withhold a newer list from a client that never saw it;
  - show different clients different lists that were signed concurrently at
    the same seq.

  Checkpoints stop any later rollback or substitution; detecting such a
  split view needs a consistency mechanism outside this step.
- **Interface:**
  - The access picker of an encrypted private channel lists members by uid
    and offers "Add everyone with role…" (expanded when signed).
  - It shows who signed the current list and when.
  - "Who can read" is unchanged: the group's devices.

### Tests and acceptance

- **6a, server:**
  - hiding in the Welcome and in every event (`client.*` with masking,
    `voice.talking`, `voice.slot`, chat, read marks), checked on raw frames,
    for every admission path (role, user, admin);
  - admission loss while inside (moved out, `channel.hidden`, occupants
    masked) and gain (`channel.created`, occupants shown), including a
    stationary occupant during another session's admission change;
  - entry and moves refused, and message-id requests answering `not_found`;
  - no sibling-name oracle (create and rename succeed beside a hidden
    sibling);
  - attachments of a private channel refused without admission in Link and
    Members modes, including a retained URL after losing admission;
  - role grants: a role the granter does not hold (Admin included) cannot
    be given out through invites, tokens or `member.groups`, at issuance or
    at redemption, by a member holding every permission through other
    roles; the outcome of a grant is the same before and after the role
    starts admitting a hidden channel;
  - no password, no children, never the default channel;
  - TeamSpeak: the bridge sees nothing (unit test on bridge notes), plus the
    official-server CI probe.
- **6a, web:**
  - tombstones: `channel.hidden` then `channel.created` keeps the channel's
    identity and messages unqualified, and `channel.deleted` then reappearance
    qualifies them;
  - e2e: a role-based private channel is visible to role members only, and
    removing the role hides it live.
- **6b:**
  - golden and rejection vectors for every statement;
  - chain extension and refusal (wrong key, wrong previous hash, gaps,
    forks, a lower seq);
  - the first-token genesis only, atomic with the Admin grant, and only
    while there is no owner (an unused first token after `owner set` or
    `owner reset` grants Admin only);
  - `owner set` only while there is no owner;
  - `owner reset` as a new generation with checkpoints cleared on
    acceptance;
  - anchor and list validity, bounds, and confirmer and manager authority;
  - the anchor's key: a fresh reader verifies a confirmer-signed list at
    genesis and right after a transfer from the proof objects alone;
  - a former owner's anchor with a higher `anchor_seq`, signed after the
    transfer and after the reader accepted the new owner's anchor, is
    rejected as current, and an anchor handed over by hash in the link is
    accepted;
  - a transfer whose `anchor_hash` is no longer the server's anchor (X
    replaced by Y, or a first anchor published, between signing and
    submission) answers `conflict`;
  - history after anchor rotation followed by a transfer (X → Y → transfer
    naming Y) still verifies commits and owner-signed lists under X;
  - the new owner renews an inherited anchor's list; the former owner's list
    after the move is refused as current, by readers and by
    `managers.update`;
  - `managers.update` racing `owner.transfer` in both orders (validated
    before the transfer, stored after it) never installs a pair that is not
    current under the new head;
  - a fresh reader and one that missed many updates verifying the newest
    pair;
  - currency and renewal through another confirmer after the signer's
    revocation;
  - pending and stale reporting, excluding the owner and confirmers.
- **6c:**
  - extension parsing and the tree rule;
  - GCE commits: only `0xff01` changes; seqs monotonic; signer authority;
    removals in the same commit; certificates at commit time; never in an
    external commit;
  - the observer and the agent agree;
  - the current-authority gate (a withheld manager propagation blocks
    sending until a member carries it; a backdated list signed by a now
    revoked device does not become authoritative);
  - offers committed by an ordinary member, and repair after expiry with the
    manager's group state kept and lost;
  - a newcomer refused with non-current lists;
  - restart authority, with the successor's seq 1 accepted, and catch-up
    G0 → G1 (created by a manager removed later) → G2 (by the owner);
  - a removed manager's backdated successor rejected with the predecessor's
    state, outbox and pins unchanged;
  - a hidden-then-shown channel cannot roll back its member list;
  - extension size limits at 1000 members and 100 managers;
  - voice under a private parent, and access removal cutting voice;
  - an active call held at once when a newer manager pair is accepted while
    its propagation into the parent is withheld and repair awaits a manager;
  - web e2e: the owner and a manager create a private encrypted channel, add
    a member by role expansion, remove them (removed from the group and the
    view), and the member list survives a restart.
