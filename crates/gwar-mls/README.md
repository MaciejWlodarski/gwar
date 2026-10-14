# Device agent

`gwar_mls::Agent` implements the client part of MLS core. It has no wasm
or networking dependency. The observer and agent share structural validation
in `rules.rs`; both reject restart descriptors until the channels step specifies
the predecessor chain.
Message encryption (`chat`), server delivery, IndexedDB and Web Locks adapters,
and web UI integration belong to later steps.

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
settle(server: &str, id: &[u8; 32], winner: &[u8; 32]) -> Result<bool>
welcome(server: &str, id: &str, bytes: &[u8]) -> Result<WelcomeResult>
join_external(server: &str, bytes: &[u8], now: i64,
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

// Queries on &Agent:
device_hpke_key() -> DeviceHpkeKey
creation(server: &str, id: &[u8; 32]) -> Result<Vec<u8>> // GroupInfo while Creating
group(server: &str, id: &[u8; 32]) -> Option<&GroupRecord>
epoch(server: &str, id: &[u8; 32], epoch: u64) -> Option<&EpochRecord>
outbox(server: &str, id: &[u8; 32]) -> Option<&Outbox>
gate(server: &str, id: &[u8; 32], now: i64,
     revocations: &dyn Revocations) -> Result<Gate>
sending_key(server: &str, id: &[u8; 32], now: i64,
            revocations: &dyn Revocations) -> Result<(u64, [u8; 32])>
```

`create` returns a TLS MLS GroupInfo message in persisted `Creating` state.
Until it is settled, `gate.status` is `creating` (`rejoin` is false), and
sending, commits and history sharing are blocked; incoming commits, proposals,
Welcomes and external joins cannot activate it. Register it with
`mls.group.create`, then call `settle(server, id, winner)` with the group the
server holds for the conversation: the `{group}` reply, or the group named by
`conflict`. If `winner` is this group (also when a lost reply or a restart made
the device register it again), it becomes **Active** with epoch zero derived and
`settle` returns `true`; catch up through the log from there. Otherwise its
OpenMLS group and record (pin) are deleted and it returns `false`: persist
that, then join the winner by Welcome or external commit. A restarted device
re-exports the GroupInfo of a `Creating` group with `creation`. Settling a
group outside `Creating` is an error.

Key packages are bare TLS
KeyPackages; commits, GroupInfos and Welcomes are TLS MLS messages. `Outbox`
contains the source epoch, group id, SHA-256 of the commit, commit bytes,
optional Welcome and resulting GroupInfo. `WelcomeResult` gives the group id
and a `duplicate` flag; acknowledge both results after persistence.

## Operation and persistence contract

Acquire `gwar-mls:{device_key}` with Web Locks before opening the connection.
Load all four stores from `gwar-mls-v1-{device_key}` in one transaction, then
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
| `groups` | UTF-8 `{server_key}/{group_id_base64url}` | Descriptor bytes, status, checkpoint, Welcome ids, outstanding-proposal flag |
| `groups` | UTF-8 `@device` | Version 1, device key, HPKE private/public key and statement creation time |
| `epochs` | UTF-8 `{server_key}/{group_id_base64url}/{epoch:020}` | Derived key and UID evidence, imported key and sender UIDs, or conflicted |
| `outbox` | Same key as group | Pending commit and its delivery artifacts |

Gwar values use strict UTF-8 JSON with unknown fields rejected. Epoch fields
use canonical decimal strings; epoch keys use exactly 20 zero-padded decimal
digits representing a u64. Loading rejects noncanonical group keys, epoch or
outbox keys without an exact known group prefix, and malformed epoch suffixes.
The device record must contain a matching X25519 private/public key pair and
a positive safe-integer creation time, also used in the wasm HPKE statement.
Byte fields are arrays. Epoch provenance uses externally tagged variants:
`{"derived":{"key":[...],"uids":[...]}}`, `{"imported":{...}}`, or `"conflicted"`.
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
pins and freshness. All group contexts must have incarnation zero; restarts
return `Invalid("restart")` through the shared rule error until step 4. They
require no admission proof or server time. Member
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
operations or invents server acceptance.

History sharing verifies the recipient's signed HPKE statement and membership.
History-on shares all nonconflicted held keys. History-off shares only locally
derived keys whose UID evidence contains the recipient. Chunks span at most
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

## Validation commands

Local validation with Rust 1.99.0, using
`CARGO_TARGET_DIR=/tmp/astra-p4-agent-fix-target`:

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude vc-desktop --all-targets -- -D warnings
cargo test -p gwar-mls
cargo test --workspace --exclude vc-desktop --exclude vc-client
node scripts/mls-wasm/test-build.mjs
scripts/mls-wasm/build.sh # pinned tools and frontend dependencies; see wasm README
node scripts/mls-wasm/smoke.mjs
```

All commands passed for round 2. The MLS suite has 55 unit tests and 11
observer integration tests. The workspace suite passed 353 tests, with 5
existing TeamSpeak tests ignored. Regressions cover persisted creation
acceptance, settling a lost reply and a lost creation, complete eviction
cleanup (including a losing pending commit and referenced proposals), pruned
membership and external-join cleanup, unrelated storage preservation, and
the 480,000-byte aggregate gzip limit.

The production wasm artifact measured with Node **22.23.3** totals **450,867
bytes gzip**: wasm 447,824, minified loader 2,852, inline JavaScript 191.
The 480,000-byte budget leaves 29,133 bytes of headroom for message encryption.
The build uses Rust 1.99.0, wasm-bindgen 0.2.129 and Binaryen 123.0.0.
On this macOS host, the wasm build needed the installed rustup toolchain's
`bin` ahead of Homebrew Rust (which lacked the wasm target), plus a local
linker wrapper setting `DYLD_LIBRARY_PATH` to that same toolchain's `lib`
directory so `rust-lld` could find `libLLVM.dylib`. These are local environment
adjustments; CI uses the pinned tools in its Linux job.

The Node smoke test passed creation settlement (won and lost), eviction cleanup,
commit recovery, Welcome deduplication, matching exporters, history and rollback.
No live Gwar or TeamSpeak server was contacted; workspace integration tests
use their own local test services. Remote CI has not been run for these fixes.
Message seal/open, server delivery and browser storage/UI wiring remain
outside this task.
