# Browser MLS runtime

`MlsRuntime` owns one unconnected Gwar `Link`, the device Web Lock, the
in-memory wasm agent and its IndexedDB database. It is deliberately not wired
into the controller or DM UI yet. TeamSpeak links must not use it.

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

`openMessage` authenticates the envelope and checks the inner context; it does
**not** verify the v3 author's signature, revision continuity or revocations.
The UI step must extend/use `MessageVerifier` for encrypted message contexts
before rendering or notifying. Today's plaintext verifier rejects non-dash
group/epoch fields. The runtime sends only MLS protocol requests, never chat
messages. No existing plaintext message send path changes here.

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
`scripts/mls-wasm/build.sh`. Normal UI bundles do not reference this module
until the UI integration step. An unsupported Web Locks environment fails
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

The database is `gwar-mls-v1-{device_key}` with `openmls`, `groups`, `epochs`,
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
  outbox write precede sending; retries reuse identical chunks for server
  deduplication. A job is deleted only after all chunks were accepted, or its
  recipient ceased to be a member. The gate is checked again before retries.

Creating groups settle against the complete paginated `mls.groups` snapshot.
Welcomes are acknowledged only after durable membership: known delivery ids
use core deduplication, and an already active membership supersedes a late
Welcome for that group without replacing its epoch. Pending external commits
are reconciled first. Log pages process their referenced proposals before
commits, then the head's pending proposal set. Noncurrent leaves are removed
by a member commit; an own removal requires external rejoin.

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
epoch keys, message opening, history import, restart and device-lock handoff.
Build `cargo build -p vc-server --no-default-features` and the wasm first;
install Chromium using `pnpm --filter web exec playwright install chromium`.
The `mls-wasm` CI job runs this test. No production or TeamSpeak service is used.
