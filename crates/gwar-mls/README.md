# Device agent

`gwar_mls::Agent` implements the client part of MLS core. It has no wasm
or networking dependency. The observer and agent share structural validation
in `rules.rs`; restarts are accepted along their chain (below).
Message encryption is in `chat`. The browser delivery, IndexedDB and Web Locks
adapter is documented in [the web runtime](../../apps/web/src/mls/README.md).
The DM UI integration remains a separate step.

## Native API

All fallible methods return `Result<T, agent::AgentError>`. Server keys are
canonical base64url Ed25519 public keys, group ids are raw 32-byte hashes,
times are caller-supplied Unix milliseconds within the JavaScript safe-integer
range, and epochs are `u64`. Device creation time must also be greater than zero.

```rust
Agent::new(seed: [u8; 32], credential: Credential, now: i64) -> Result<Agent>
Agent::load(seed: [u8; 32], credential: Credential, stores: Stores) -> Result<Agent>

// Methods on &mut Agent:
take_changes() -> Changes
key_packages(n: usize, now: i64) -> Result<Vec<Vec<u8>>>
last_resort_key_package(now: i64) -> Result<Vec<u8>>
create(descriptor: SignedDescriptor) -> Result<Vec<u8>> // GroupInfo; Creating
settle(server: &str, id: &[u8; 32], winner: Option<&[u8; 32]>) -> Result<bool>
welcome(server: &str, id: &str, bytes: &[u8]) -> Result<WelcomeResult>
replace_stale_welcome(server: &str, group: &[u8; 32], id: &str,
                      bytes: &[u8]) -> Result<WelcomeResult>
join_external(server: &str, bytes: &[u8], tree: Option<&[u8]>, now: i64,
              revocations: &dyn Revocations) -> Result<Outbox>
commit(server: &str, id: &[u8; 32], adds: &[Vec<u8>], removes: &[u32],
       now: i64, revocations: &dyn Revocations) -> Result<Outbox>
receive(server: &str, id: &[u8; 32], commit: &[u8],
        proposals: &[Vec<u8>]) -> Result<()>
proposal(server: &str, id: &[u8; 32], bytes: &[u8]) -> Result<()>
pending(server: &str, id: &[u8; 32], pending: bool) -> Result<()>
reconcile(server: &str, id: &[u8; 32], result: Reconcile) -> Result<Resolution>
share_history(server: &str, id: &[u8; 32], recipient: &DeviceHpkeKey,
              now: i64, revocations: &dyn Revocations) -> Result<Vec<Vec<u8>>>
import_history(server: &str, id: &[u8; 32], bytes: &[u8]) -> Result<()>
forget(server: &str, id: &[u8; 32]) -> Result<()>
accept_chain(server: &str, conversation: &str,
             chain: &[([u8; 32], Vec<u8>)]) -> Result<[u8; 32]>

// Queries on &Agent:
inventory(server: &str) -> Result<Vec<GroupSummary>>
device_hpke_key() -> DeviceHpkeKey
group(server: &str, id: &[u8; 32]) -> Option<&GroupRecord>
epoch(server: &str, id: &[u8; 32], epoch: u64) -> Option<&EpochRecord>
outbox(server: &str, id: &[u8; 32]) -> Option<&Outbox>
gate(server: &str, id: &[u8; 32], now: i64,
     revocations: &dyn Revocations) -> Result<Gate>
sending_key(server: &str, id: &[u8; 32], now: i64,
            revocations: &dyn Revocations) -> Result<(u64, [u8; 32])>
behind(server: &str, id: &[u8; 32], group_info: &[u8], tree: Option<&[u8]>) -> Result<bool>
```

`create` returns a TLS MLS GroupInfo message in persisted `Creating` state.
Until it is settled, `gate.status` is `creating` (`rejoin` is false), and
sending, commits and history sharing are blocked; incoming commits, proposals,
Welcomes and external joins cannot activate it. Register it with
`mls.group.create`, then call `settle(server, id, winner)` with the group the
server holds for the conversation: the `{group}` reply, or the group named by
`conflict`. After a restart (or a lost reply), take it from `mls.groups`
instead, `None` if the conversation has no group. If `winner` is this group,
it becomes **Active** with epoch zero derived and `settle` returns `true`;
catch up through the log from there. Otherwise its OpenMLS group and record
(pin) are deleted and it returns `false`: persist that, then join the winner
by Welcome or external commit, or create the conversation again when there is
none. Settling needs no certificate check, so it also works after the
creator's leaf expired. Settling a group outside `Creating` is an error.

Key packages are bare TLS
KeyPackages; commits, GroupInfos and Welcomes are TLS MLS messages. `Outbox`
contains the source epoch, group id, SHA-256 of the commit, commit bytes,
optional Welcome and resulting GroupInfo. `WelcomeResult` gives the group id
and a `duplicate` flag; acknowledge both results after persistence.

## Chat envelope

`chat::seal(&Agent, &Context, plaintext: &[u8], now, revocations)` returns nonce,
ciphertext and tag bytes. It obtains the key through `sending_key`, checks
that the context names the descriptor's conversation and current epoch, then
uses HKDF-SHA256 with the raw 16-byte message id and big-endian u32 revision,
and AES-256-GCM with a fresh random 96-bit nonce. The AAD is exactly the
versioned statement in the build spec.

`chat::open(&Agent, &Context, ciphertext)` returns `Vec<u8>` or `ChatError`.
Seal encrypts the input JSON bytes unchanged; open returns those exact bytes.
Both validate the entire input as UTF-8 and parse only the six signature context fields, avoiding the full protocol
message/attachment/certificate types in wasm. Open uses derived or imported
epoch keys and checks the inner signature's server, conversation, group, epoch, message id and revision. Missing groups
or keys return `NoKey`. Open rejects AEAD authentication failures, invalid UTF-8,
JSON syntax errors and missing, malformed or mismatched signature context fields.
Other plaintext fields are skipped by this parser.
The caller **must parse/validate the full plaintext and verify its v3
MessageSignature**, including revision and revocation rules. This module does not assert author authenticity.

`inventory(server)` returns each group's id, conversation, incarnation,
whether it is the live pin, history policy, record and current member device
keys/uids, including Creating and Superseded groups. It
allows the runtime to settle creations and atomically queue history intents
when an accepted transition admits devices without parsing OpenMLS records.

## Operation and persistence contract

Acquire `gwar-mls:{device_key}` with Web Locks before opening the connection.
Load all four stores from `gwar-mls-v2-{device_key}` in one transaction, then
construct the agent. A new agent instead creates its HPKE key: persist its
initial `take_changes()` before its first operation.

1. Run one mutation synchronously to completion.
2. Call `take_changes()` once. Write every returned change in one IndexedDB
   transaction across all four stores. `value: None` means delete.
3. Only after that transaction completes, send the operation's output or
   acknowledge a Welcome. Then begin the next mutation.
4. On an operation error or failed transaction, discard the agent and reload
   the last durable stores. Never persist a partial operation.

A failed core mutation poisons the agent and exposes no changes, even if
OpenMLS consumed a key package or wrote intermediate state. A second mutation
before `take_changes()` also poisons it. The agent cannot observe the caller's
transaction result: taking changes does **not** mean the write succeeded.
`Changes::apply` provides the equivalent in-memory store update for native
callers and tests. Finish the current write before releasing the Web Lock.

| Store | Key | Value |
| --- | --- | --- |
| `openmls` | Opaque binary OpenMLS label, encoded key, storage version | Opaque OpenMLS record |
| `groups` | UTF-8 `{server_key}/{group_id_base64url}` | Descriptor bytes, status, checkpoint, Welcome ids, outstanding-proposal flag, evidence intervals per uid, own last commit time |
| `groups` | UTF-8 `@device` | Version 1, device key, HPKE private/public key and statement creation time |
| `epochs` | UTF-8 `{server_key}/{group_id_base64url}/{epoch:020}` | Derived key, imported key and sender UIDs, or conflicted |
| `outbox` | Same key as group | Pending commit and its delivery artifacts |

The web runtime also owns `@runtime/` outbox keys for package publication and
history jobs; its adapter filters that namespace before `Agent::load`. Core
load continues to reject any outbox key without a known group.

Gwar values use strict UTF-8 JSON with unknown fields rejected. Epoch fields
use canonical decimal strings; epoch keys use exactly 20 zero-padded decimal
digits representing a u64. Loading rejects noncanonical group keys, epoch or
outbox keys without an exact known group prefix, and malformed epoch suffixes.
The device record must contain a matching X25519 private/public key pair and
a positive safe-integer creation time, also used in the wasm HPKE statement.
Byte fields are arrays. Epoch provenance uses externally tagged variants:
`{"derived":{"key":[...]}}`, `{"imported":{...}}`, or `"conflicted"`. Evidence
maps each uid to sorted, disjoint, non-adjacent inclusive epoch ranges
`[{"first":"0","last":"3"}]` in which it had a leaf, as this device saw them;
loading rejects any other shape.
Sets are sorted. Group descriptors retain conversation/incarnation and the
first accepted group id, including after eviction, so they also implement pins.
Checkpoints hash the TLS GroupContext. The device seed and current credential
are supplied by the identity layer on every load, not stored a second time.
All stores contain secrets and belong inside the device's local storage boundary.

OpenMLS records use postcard 1.1.3 with exact-consumption decoding, except
join configuration and message secrets, whose upstream untagged policy enum
requires JSON. This is a pinned OpenMLS 0.9.1 format, not a public wire format.
There is no migration or backward compatibility. The storage adapter keeps
records in memory; `take_changes` compares them and Gwar records to the last
snapshot, returning only final additions, changes and deletions. This favors
simple atomic recovery over an incremental journal, at the cost of copying
and comparing the device's stores per mutation.

## Acceptance, sending and recovery

Welcome and external joins validate the resulting tree structurally, with
pins and freshness, and require no admission proof or server time. Only the
live pin, the highest accepted incarnation of a conversation, may be joined
(`superseded` below it, `restart chain` above it or, without a pin, above
zero). `accept_chain` verifies a conversation's restarts from the live pin,
or from incarnation zero without one: each descriptor parses and verifies,
names its group, server and conversation, has a creator certified at its
creation time, and continues the one before (next incarnation, its group as
`previous_group_id`, the same kind, `private`, `history`, owner and channel
group). It records every new incarnation (the last as `Rejoin`, to be
joined; this device's own creation there is settled as won) and supersedes
the earlier ones in the same operation. `create`
also takes a restart candidate: the next incarnation of the live pin, one per
predecessor; settling it as the winner supersedes the predecessor.
**Superseded** groups keep their epoch keys and evidence for reading; their
OpenMLS state, private epoch key pairs and outbox are gone, and nothing can
be sent, committed or imported there. `behind` verifies a GroupInfo of the
named group as an external joiner would (leaf lifetimes skipped) and reports
whether it is behind the group's checkpoint or forks it, a restart trigger. Member
staged commits additionally check introduced/changed certificates at the time
in signed `ds::commit_aad`. Later revocation or expiry never rejects an otherwise
valid commit. Joins skip OpenMLS leaf lifetime checks; key packages expire no
later than the certificate and within 84 days.

Call `gate` after every join or merge with the caller's current time and
revocation view. Its `blocked` field reports noncurrent leaf indices;
`commit` includes their removal automatically. `sending_key` and `share_history`
recompute the gate. An own-leaf removal proposal or noncurrent own leaf requires
rejoining. Load with the renewed credential before committing an update while
the old certificate is still current; every member commit publishes the loaded
credential in its path leaf, and external joins already use it for their leaf.
A normal pending member commit retains the prior accepted epoch;
`pending_commit` reports it separately. A joining external commit exports no
epoch key and cannot send until accepted.

Set `pending(true)` when the server announces outstanding proposals. Fetch
and feed the signed proposals to `proposal`, then clear that notification
latch with `pending(false)` after the complete pending set is known. The stored
proposals themselves continue blocking sends until the epoch advances.
Commit creation refuses to proceed while the notification latch is set.
Referenced proposals supplied with a logged commit are processed before it.

Voice uses a separate volatile agent (`new_voice` / `load_voice`). Every external
join, including snapshot recovery after a log gap, starts a fresh membership trust
interval: old removal holds are discarded, but export remains blocked after join
acceptance. Catch up through `mls.log.head`, then pass that head and the **complete**
`mls.log.pending` set to `voice_pending(server, id, head, proposals)`, even when it
is empty. This atomically validates/stores proposals and clears the recovery
barrier only at the matching active epoch with no outstanding own commit. Persist
its changes to the volatile checkpoint before export. Retry a stale response;
individual proposal events, `pending(false)`, and later commits cannot clear this
barrier. Reissued removals create new holds that survive competing commits and
clear only on an observed removal or another reconciled external join.

An authenticated leaf fingerprint change cannot prove a removal: an ordinary
self-update changes it too. Pending-state reconciliation instead trusts the server
to report every detached leaf, as uninterrupted participation already trusts
server proposal delivery. It does not detect a server withholding removals. The
existing server reissues them after each voice commit and includes the complete
current set in every log response; no new wire message is needed.

Feed authoritative log information to `reconcile`:

| Result | Action |
| --- | --- |
| `Entry` at source epoch with matching commit hash | Merge pending state, derive key; `Accepted` |
| Different `Entry` at source epoch | Discard pending member commit, process winner; `Rebuild` or `Rejoin` if removed |
| `Head` equal to source epoch | Keep and resend identical bytes; `Retry` |
| `Rejected(Expired)` | Discard and rebuild; `Rebuild` |
| Any other rejection | Keep and retry with caller backoff; `Retry` |
| `Pruned` with oldest epoch later than source | Delete OpenMLS state and rejoin; `Rejoin` |

A losing or expired external join is deleted and returns `Rejoin`. Pins,
checkpoints, Welcome ids and old keys survive deleting OpenMLS membership.
After validating an own-leaf removal, eviction deletes the OpenMLS group
before advancing its epoch, so its old private epoch key pairs are deleted too.
Eviction, pruned recovery, losing/expired external joins and lost creations leave
no OpenMLS records for that group. Eviction keeps the last member epoch
checkpoint and application epoch keys; rejoining requires a newer GroupInfo.
Rebuilds use the current time and revocation view. No method retries network
operations or invents server acceptance. If a winning log entry cannot be
replayed (for example an expired Add key package), `join_external` can replace
a pending commit only from a validated GroupInfo **strictly newer than its
source epoch**. Equal/older heads cannot discard the outbox. Invalid newer
GroupInfos poison the operation without exposing changes, so reload retains
the pending bytes. A successful recovery is still Joining with no new sending
key until its external commit is accepted.

If a pruned log prevents replay of stale active/joining membership, the caller
can first try `replace_stale_welcome`. This shares normal Welcome acceptance
rules, requires the expected group and an epoch strictly newer than the local
checkpoint and pending commit source, and atomically replaces OpenMLS membership
and the outbox while retaining old application epoch keys. Persist the derived
Welcome epoch and delivery id before acknowledging. Invalid Welcomes expose no
changes; reload durable state before retrying or falling back to external rejoin.

`forget` deletes everything held for a group whose conversation no longer
exists (a deleted channel): its OpenMLS state with every epoch's encryption key
pairs, outbox, epoch keys, evidence and pin. Forgetting an unknown group does
nothing.

`own_commit_at` in a group record is the commit time of this device's last own
commit there that the log accepted (a creation counts at its descriptor time);
it is `None` after a Welcome. Channel groups use it for weekly leaf updates.

History sharing verifies the recipient's signed HPKE statement and membership.
History-on shares all nonconflicted held keys. History-off shares only locally
derived keys of epochs in which the recipient's uid had a leaf, by the group's
evidence intervals. Chunks span at most
1000 epochs. Imports require a sender in the recipient's current tree; derived
keys always win, equal imports accumulate sender provenance, and conflicting
imports permanently remove that imported key regardless of arrival order.

## Implementation choices and validation

The crypto adapter restricts the standard RustCrypto/HPKE primitives to Gwar's
single suite and reuses the existing SHA-256 implementation. This avoids linking
unused algorithms and duplicate hash implementations into wasm. Conformance
checks compare it with the unmodified provider, including HKDF errors and
HPKE sender/receiver exporters in both directions; existing history vectors and
observer interoperability tests remain unchanged. The storage adaptation's
license is in `THIRD_PARTY_NOTICES.md`.

Limits are 20 generated key packages per operation, 4 KiB incoming key packages,
and 128 KiB handshake messages. Time and revocations remain caller inputs;
there is no clock or network service in the agent. Gate reporting is a separate
query so Welcome keeps its bytes/id API without adding time or proof parameters.

Native tests use multiple agents and `ds::Observer`, exercising commits, joins,
revocation, removals, pins, freshness, crash recovery, all outbox cases, history
policy/conflicts, lifetime handling and persistence. The wasm smoke test runs
in Node with in-memory stores and a simulated authoritative acceptance result;
it is not browser IndexedDB, Web Locks or server-integration coverage.

## Validation

```sh
cargo fmt --all --check
cargo clippy -p gwar-mls -p gwar-mls-wasm --all-targets -- -D warnings
cargo test -p gwar-mls -p gwar-mls-wasm
node scripts/mls-wasm/test-build.mjs
scripts/mls-wasm/build.sh # pinned tools; see wasm README
node scripts/mls-wasm/smoke.mjs
pnpm --filter web lint
pnpm --filter web test
pnpm --filter web build
cargo build -p vc-server --no-default-features
pnpm --filter web e2e:mls
```

The native suite has 78 unit tests and 11 observer integration tests. Chat
coverage includes an independently generated Node HKDF/AES-GCM fixed vector,
every AAD/inner context field, random nonces, wrong keys, every truncation,
tampered nonce/ciphertext/tag, the sending gate and imported history keys.
The wasm smoke test also covers chat and inventory. The browser test uses the
real delivery service, four device runtimes, IndexedDB and Web Locks, with no
DM UI or chat.sealed server calls. Remote CI and production were not run.

With Rust 1.99.0, Node **22.23.3**, wasm-bindgen 0.2.129 and Binaryen 123.0.0,
the artifact totals **476,253 bytes gzip**: wasm 473,205, minified loader 2,856,
inline JavaScript 192; 3,747 bytes below the limit of that time (480,000
bytes; 500,000 since large groups).
On this macOS host the wasm build uses the rustup toolchain's `bin` ahead of
Homebrew Rust and a local rust-lld wrapper setting `DYLD_LIBRARY_PATH` to that
toolchain's `lib` directory. CI uses the pinned Linux job.
