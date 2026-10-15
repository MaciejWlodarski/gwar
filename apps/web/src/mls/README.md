# Browser MLS runtime

`MlsRuntime` owns one unconnected Gwar `Link`, the device Web Lock, the
in-memory wasm agent and its IndexedDB database. The application controller
owns its lifecycle and `state/sealed.ts` wires it to encrypted direct
message and channel state and the chat UI. TeamSpeak links must not use it.

```ts
const runtime = await MlsRuntime.connect({
  identity,
  seed, // Uint8Array decoded from this device's Ed25519 JWK d
  link, // new Connection(...), still idle; runtime calls connect/close
  revocations: () => currentRevocations,
  onError: reportBackgroundFailure,
});
await runtime.openConversation(conversation); // canonical dm:{uid}:{uid}
const unsubscribe = runtime.subscribe(renderStatusAndRetryMissingMessages);
const status = runtime.status(conversation); // ready | securing | creating

const sealed = await runtime.sealMessage(conversation, async context => {
  // Build/sign a v3 MessageSignature using exactly context.server_key,
  // context.conversation, context.group_id and context.epoch. The callback
  // runs inside the serial queue; keep message_id/revision stable on retry.
  return { text, mentions, attachments: [], signature: await sign(context) };
}); // { group: base64url, epoch: safe integer, ciphertext: base64url }

const result = await runtime.openMessage({
  conversation, group, epoch, message_id, revision, ciphertext,
}); // { ok: {text, mentions, attachments, signature} }
    // or { reason: "no_key" | "cannot_open" }

unsubscribe();
await runtime.close();
```

`openMessage` authenticates the envelope, checks all six inner context fields,
parses the full plaintext and verifies its body hash, device certificate and
v3 author signature. Step 3 rejects attachments. It uses the agent query path
without an IndexedDB write. Invalid envelopes/plaintext/authentication return
`cannot_open`; absent keys return `no_key`; storage, runtime, closed-session
and operational crypto failures reject the promise. The UI uses
`MessageVerifier` for encrypted message contexts to apply revision continuity
and revocation rules before rendering or notifying. Plaintext verification
still rejects non-dash group/epoch fields; sealed verification requires the
exact outer context. The runtime sends only MLS protocol requests, never chat
messages. Channel/server chat and TeamSpeak DMs keep their plaintext paths.

**Channel groups.** The runtime reads the encrypted channels from every
Welcome and from `channel.created`/`channel.deleted`, and keeps their groups
without being asked (docs/e2e-build.md "Groups", "History in channels"):

- a group the server does not list yet is created after a random delay of up
  to five seconds, or right away after `channelCreated(id)`, which the client
  that created the channel calls;
- the device joins every other channel's group by external commit, one at a
  time with up to two seconds of jitter between joins, and then calls
  `mls.history.request`: the intent is written with the join and kept until a
  request succeeds (a `rate_limited` one is retried after a minute); it never
  shares on its own after an admission, but
  answers the server's assignments (`mls.history.wanted` with `assignment`)
  with chunks that carry the assignment, or `mls.history.decline` when it has
  nothing the recipient may receive;
- a failing channel backs off (5 seconds doubling up to 10 minutes) without
  holding up direct messages, and so does a history job whose recipient
  lookup or chunks fail, without holding up the other jobs;
- it commits an update of its own leaf in a channel group whose last own
  commit is older than seven days, one group per pass;
- the group of a deleted channel is forgotten: the agent deletes its OpenMLS
  state, every epoch's keys, outbox, evidence and pin, and the runtime's jobs
  for it go in the same write;
- restarts (docs/e2e-build.md, "Restarts"): a live group the server lists at
  a higher incarnation is accepted through `mls.group_history` from the live
  pin before it is joined; a conversation whose server holds an older
  incarnation or a sibling of the pin is left alone (no sync, history
  sharing or key requests) and stays "securing"; two checks at least a
  minute apart that find the server behind the pin's checkpoint (a log head
  behind it, or a GroupInfo behind or forking it when rejoining) create the
  next incarnation, and a verified GroupInfo at or past the checkpoint, or a
  log entry merged from it, cancels them.

`roster(conversation)` reports the live group (its id and whether it was
restarted), its devices by uid, its history option and the first epoch the
device saw its own account in: the chat shows "Who can read" from it,
collapses rows from before that epoch in a channel without shared history,
and says quietly when the conversation was restarted.

`sync()` is also public for explicit retries and deterministic tests. Connect
performs the first synchronization, reconnect Welcomes and MLS events schedule
another, and a 15-second maintenance pass covers clock/revocation changes,
package refill and retries. Synchronization errors leave the live runtime available for retries, keep a
conversation securing, notify
`onError` for background work, and retry with delay. Pending commits retry the
same bytes until authoritative reconciliation says accepted, replaced, expired
or pruned. Commit success replies are followed by a log read before merging.
Requests are paced below the server connection's 20 requests/second limit,
with a small burst, leaving capacity for other features using the link.

The module dynamically imports the generated wasm package on the first
`connect`, after acquiring `gwar-mls:{device_key}`. Build it first with
`scripts/mls-wasm/build.sh`. Web and desktop builds require the generated package before typechecking
or bundling the application. An unsupported Web Locks environment fails
explicitly; there is no unlocked fallback. An optional AbortSignal cancels a
waiting lock acquisition or closes an established runtime. `close()` prevents
queued work from starting, closes the connection, drains the running operation
and transaction, then releases the lock. A generation fence prevents work
started on a lost connection from sending on its replacement. Per-group event
versions also prevent a proposal/commit arriving during asynchronous signing
or a storage write from releasing stale ciphertext. History sends check that
fence and the current revocation gate immediately before each request.
Renewed device credentials require closing and reconnecting the runtime with
the renewed identity before the old certificate expires.

The database is `gwar-mls-v2-{device_key}` with `openmls`, `groups`, `epochs`,
and `outbox`. All four are loaded in one transaction. Each wasm mutation ends
with `take_changes()` and one transaction with `durability: "strict"`. The
engine waits for transaction completion before sending or acknowledging. Any
operation or transaction failure discards the agent and reloads durable state.
An unreplayable log (for example an expired Add key package) reloads first,
then validates a fresh GroupInfo and rejoins; history recovers older keys.

The runtime reserves UTF-8 `@runtime/` keys in **outbox** and filters only
that namespace out before passing records to `Agent.load`. Core commit outbox
keys retain their exact server/group form. Runtime entries are JSON bytes:

- `@runtime/packages/{server}`: generated packages and last-resort bytes,
  plus their conservative expiry. A lost publish response retries the same
  bytes. Expired unpublished jobs are replaced with current packages.
- `@runtime/history/{server}/{group}/{recipient}`: a share intent (`chunks:
  null`), or the sealed chunk list. Newly admitted devices are detected by
  comparing the accepted membership before/after a mutation. Their intents
  are committed in the **same transaction** as the accepted state, including
  commits accepted through outbox reconciliation. Chunk generation and its
  outbox write precede sending. Sealed jobs include `publication`, the verified
  recipient `{device_key, hpke_key, created_at, signature}` (`null` for an
  intent). Every retry fetches the recipient's current publication: a strictly
  newer one is verified by the core and resealed chunks replace the whole job
  atomically before sending. Failed validation or storage preserves the old
  job. Unchanged/older publications reuse identical bytes for server
  deduplication, including when another `wanted` request arrives. A job is
  deleted only after all chunks were accepted, or its recipient ceased to be a member. The gate is checked again before retries.

Creating groups settle against the complete paginated `mls.groups` snapshot.
Welcomes are acknowledged only after durable membership: known delivery ids
use core deduplication. Before declaring an unfamiliar Welcome superseded,
the runtime replays the log (and reconciles pending commits), stopping at
eviction. If the log was pruned before local membership could catch up, a
readmission Welcome can replace stale active/joining membership only for the
same group at a strictly newer epoch, using the core's shared Welcome acceptance
rules. Successful imports and their delivery ids are persisted before
acknowledgement. If no usable Welcome is available, synchronization falls back
to external rejoin; skipped epoch keys may then require history sharing.
Only membership caught up to the log head supersedes a
late Welcome without replacing its epoch. Log pages process their referenced
proposals before commits, then the head's pending proposal set. Noncurrent leaves are removed
by a member commit; admission candidates and claimed adds are filtered against
the local revocation view even if the server still advertises them. An own
removal requires a readmission Welcome or external rejoin.

History fetches are acknowledged after import. Missing message epochs remain
in an in-memory demand set; requests are limited to once a minute per group
and repeated until the key arrives. Live/reconnect `history.wanted` requests
create the same durable share jobs. UI subscribers run after synchronization
so they can retry previously unopened messages; notification deduplication
belongs to the UI. Epochs in wasm and persistence are decimal u64 strings;
the existing delivery protocol uses safe JSON integers, and the runtime
rejects values it cannot represent exactly.

Tests use fake-indexeddb, an injectable agent/connection and real IndexedDB
transaction aborts. `pnpm --filter web e2e:mls` additionally starts its own
loopback-only `vc-server --no-connect` with disposable data and Vite, then
uses Chromium with the real wasm. It checks two users, four devices, equal
epoch keys, authenticated message opening, history import, restart and device-lock
handoff. Unit regressions also cover offline readmission and failed Welcome
writes, locally revoked admission and a feed advancing during claims, HPKE
rotation with failed atomic replacement, identical history retries, read-only
opening and operational failure propagation, and full v3 plaintext validation.
Build `cargo build -p vc-server --no-default-features` and the wasm first;
install Chromium using `pnpm --filter web exec playwright install chromium`.
The `mls-wasm` CI job runs this test. `e2e:dm` additionally exercises the real
controller and React chat UI with four devices. No production or TeamSpeak service is used.
