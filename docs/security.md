# Security model

Status markers: **implemented** (on `main`), **in progress** (being built),
**planned** (agreed, not started). The protocol details live in
[architecture.md](architecture.md) and [connect.md](connect.md); this document
says what each mechanism protects and what it does not.

## Starting point: everything is public

Gwar is open source. Anyone can read, modify and run the server, the web app,
the desktop app and Gwar Connect. A design is acceptable only if it stays secure
when the attacker knows every line of code and runs a modified copy of any
component other than **your own client**. Security comes only from keys that
never leave their owner, never from hidden behavior.

## Parties and what each is trusted with

| Party | Trusted for | Not trusted for |
| --- | --- | --- |
| Your client (the web app as served to you, the desktop binary) | everything: it holds your keys and makes every verification decision | — |
| Official web hosting (`gwar.maciejwlodarski.com`) | serving the genuine web app and the Connect URL to web users | — |
| Gwar Connect | mapping handles to account keys; storing encrypted blobs; publishing revocations; availability of sign-in | your password, your keys, your vault, the content of revocations (it can only withhold them) |
| A server you join | availability, routing voice, presence, ordering and delivery, its own permissions | anyone's identity (yours included), the authorship and content of messages, handles |
| Other members | nothing | — |
| The network | nothing | — |

A server may be modified by its operator, or be an attacker's server you joined
by mistake. The design treats this as the normal case.

## Assets

1. Private keys: account key, device keys, local identity keys, server keys.
2. The Connect password and the recovery code.
3. The vault (profile, TeamSpeak identities).
4. Who said what: authenticity of messages, edits and attachments.
5. Your roles on honest servers.
6. Handles (`@name`) as a human-readable identity.

Not protected (see "What a malicious server can still do"): confidentiality from
the server, presence, voice authenticity.

## Identities

- Each identity is an Ed25519 key; `uid = b64url(sha256(public_key)[..20])`.
  Nobody can choose a uid or take someone else's.
- `#tag` is derived locally from the uid; it needs nobody.
- Connect accounts have an account key, stored only encrypted; each device has
  its own key, certified by the account key. Certificates last at most 90 days
  (**planned**: enforced at issuance and by every verifier; the client asks to
  renew in the last 14 days). Revocations are signed by the account key.
- `@handle` needs Connect: see "Handles".

## Transport

- TLS (`wss://`, `https://`) is required, except to loopback.
  **Planned:** `ws://` to any other host only through a per-server opt-in in
  the desktop app, with a permanent red warning "connection can be impersonated
  or taken over". A later protocol version will replace this with a Noise
  handshake bound to the identities.
- A browser connecting by IP needs a TLS certificate valid for that IP.
- Connect is reached only over HTTPS, without following redirects
  (**implemented**).

## Logging in to a server (vc/3, in progress)

Threats: a malicious server A relays your login to an honest server B and logs
in to B as you (relay); or A pretends to be B.

Each server has an Ed25519 **server key** (`server-key.ed25519` in its data
directory; back it up, losing it means a new server identity). Login:

1. The server sends `challenge {protocol: 3, server_key, server_nonce}`.
2. The client compares `server_key` with its pin for this endpoint, or with the
   fingerprint in the invite it is using. First contact with neither is an
   explicit trust-on-first-use candidate.
3. The client signs

   ```text
   gwar client hello v3
   {endpoint}
   {expected_server_key}
   {server_nonce}
   {client_nonce}
   {client_signing_key}
   {certificate_hash}
   ```

   `endpoint` is the canonical `wss://host:port/path` the client itself dialed,
   never something the server sent. `certificate_hash` identifies the device
   certificate, or is `-` for a local identity.
4. The server accepts only its own configured endpoints (`--public-endpoint`,
   `--domain`, `--public-url`, and `--public-ip` if it is a global unicast
   address), never request headers. It checks that `expected_server_key` is its
   own, the nonce is this connection's, the signature, and that the certificate
   is current and not revoked. Then it signs
   `gwar server proof v3\n{hello_hash}\n{client_signature}`.
5. The client verifies the proof under the expected key and only then pins it.
   Server passwords and invites are sent after the proof, on the same
   connection. A missing proof, a changed transcript or another protocol
   version is fatal; there is no fallback.

Why relay fails: you signed A's endpoint and A's key; B accepts neither. Why A
cannot be B: A cannot produce a proof under B's key.

A server with no configured endpoints refuses to start unless run with
`--insecure-any-host`, an explicit development mode. Such a server can be
relayed to; pins do not fix that. Private, loopback and link-local addresses are
never accepted automatically.

## Handles (implemented)

Connect has a **service key** and signs
`gwar handle v1\n{account_key}\n{handle}\n{issued_at}\n{expires_at}`, valid seven
days. Servers fetch these attestations and pass them on. The client gets the
service key **only** from its own configured Connect URL, checks the signature,
the expiry and that the account key derives to the member's uid, and only then
shows `@handle` and the badge. A server cannot invent a handle for anyone.

Handle lookup by key is public by design.

## Connect account integrity (implemented)

- The client derives keys only with Argon2id v19, m=65536 KiB, t=3, p=1,
  64-byte output and a 16–64 byte salt, checked before deriving or sending
  anything. Connect cannot weaken the parameters to speed up guessing.
- A device only ever certifies its own locally held key. The device list from
  Connect is shown for inspection and revocation, never used as an instruction
  to sign.
- Sessions are issued against a five-minute, one-use challenge signed by the
  device (`gwar session v1 …`), bound to the service, account, certificate and
  session, and end when the certificate expires.
- Password and recovery changes are signed by the account key over the complete
  new credential set, the operation, a fresh challenge and the expected
  credential version (`gwar credentials v1 …`).
- Revocations are keyed by (account key, device key) and served from a
  monotonic cursor. Servers disconnect a session, including its voice, when its
  certificate expires or is revoked.
- Migrating an old browser's plaintext account root is crash-safe: the device
  record is saved before the root is removed.

## Messages (planned)

Every message, edit and attachment is signed by the author's session key:

```text
gwar message v2
{server_key}
{conversation}            server | channel:{id} | dm:{lower_uid}:{higher_uid}
{author_uid}
{client_signing_key}
{certificate_hash}
{message_id}              16 random bytes from the client
{revision}                0, then +1 per edit
{previous_revision_hash}  - for revision 0
{created_at}
{signed_at}
{body_hash}               text, mentions, attachment hashes
```

- The server key, not the address, binds a message to a server, so it verifies
  for everyone whether they connected by domain or by IP. Old server keys verify
  only their own history.
- Channels are named by immutable id. A direct message must involve exactly the
  two participants of the open conversation.
- Edits keep the message id and creation time and chain to the previous
  revision. A missing predecessor is shown as "continuity unproven"; two
  different revisions with the same number are a fork.
- The certificate is checked at `signed_at`. An expired or revoked device can
  still sign with a false time, so such messages show "signature valid; device
  revoked (or certificate expired); signing time unproven".
- The client remembers what it has seen (idempotent, bounded) and refuses
  rollbacks and substitutions. A fresh device has no such memory.
- Notifications are raised only for verified messages. Attachments that fail
  or lack a signature load only on request. TeamSpeak messages are shown as
  unsigned.
- Signature status, handle attestation, connection security and historical
  authorization are shown separately. A verified message does not mean the
  voice is authentic.

## Hostile input (implemented)

- Every server message is validated at runtime before use.
- File URLs are confined to the server's own origin and to
  `/files/{id}/{name}` (GET) and `/api/files/{id}?token=` (PUT), in the web app
  and in the desktop app's native code.
- Unauthenticated connections are rate limited per IP and frames are size
  limited before parsing.
- The web app has a Content-Security-Policy; responses carrying secrets are
  `no-store`.

## What a malicious server can still do

These are limits of a voice chat relayed through a server:

- **Read everything** on that server: messages, direct messages, files, voice
  (the SFU terminates DTLS-SRTP). Direct messages are not end-to-end encrypted.
- **Lie about presence**: who is online, where, who is talking.
- **Voice**: inject audio or attribute it to anyone; voice is not signed.
- **Delivery**: drop, delay, reorder or hide messages. Removing a message is
  distinguishable from the author deleting it only once deletions are signed.
- **Nicknames, channel names, roles** on that server are its word.
- **Show forged or altered content**, but only with a warning.
- **Track** your IP address and when you are online, and link your uid across
  servers (the uid is global by design).

## What a malicious Connect can do

- Attest a handle to any key; it is the authority for handles.
- Withhold revocations: a revoked device stays accepted until its certificate
  expires (at most 90 days). Revocations already seen are kept forever.
- Run an offline guessing attack on your password with the stored blob; weak
  passwords fall, the recovery code (160 bits) does not.
- Serve an older vault (rollback). A revoked device keeps the vault key it had.
- Deny service; learn which servers look up which accounts.
- It cannot sign as you, decrypt your account key without your password, read
  the vault, or forge revocations.

## Clients and the code supply chain

Every guarantee assumes your client runs genuine code.

- **Web app.** Whoever controls `gwar.maciejwlodarski.com` or its TLS can serve
  modified JavaScript that reads your keys or password; web users trust the
  hosting on every load. Local identity keys are exportable so they can be
  backed up.
- **Desktop app.** Code is bundled; the webview has a CSP. Builds are not yet
  signed or notarized; there is no auto-updater.
- **Web apps hosted by other servers** are other origins: they don't have your
  keys, but they can phish your Connect password.
- **Uploaded files** are served with `nosniff`, a sandboxing CSP and
  `Content-Disposition`, from the server's origin. Attachments are hashed before
  display or download.

## TeamSpeak

TeamSpeak users have no Gwar keys; their messages are marked as TeamSpeak and
never verified. TeamSpeak identities in the vault are shared by all of an
account's devices; revoking a device does not invalidate a copy it already has.

## Later

Noise for LAN connections, stronger evidence of signing time, vault
checkpoints against rollback, non-exportable signing keys, signed releases,
signed server key rotation, end-to-end encrypted direct messages.
