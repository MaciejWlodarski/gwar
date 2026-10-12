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
| Official web hosting (`gwar.app`) | serving the genuine web app and the Connect URL to web users | — |
| Gwar Connect | mapping handles to account keys; storing encrypted blobs; publishing revocations; availability of sign-in | your password, your keys, your vault, the content of revocations (it can only withhold them) |
| A server you join | availability, routing voice, presence, ordering and delivery, its own permissions | anyone's identity (yours included), the authorship and content of messages, handles |
| Other members | nothing | — |
| The network | nothing | — |

A server may be modified by its operator, or be an attacker's server you joined
by mistake. The design treats this as the normal case.

## Assets

1. Private keys: account key, device keys, local identity keys, server keys.
2. The Connect password and the recovery code.
3. The vault (profile, TeamSpeak identities, saved servers and their passwords).
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
  (**implemented**: enforced at issuance and by every verifier; the client asks to
  renew in the last 14 days). Revocations are signed by the account key.
- `@handle` needs Connect: see "Handles".

## Transport

- TLS (`wss://`, `https://`) is required, except to loopback.
  `ws://` to any other host works only through a per-server opt-in in the
  desktop app, with a permanent red warning "connection can be impersonated or
  taken over" (**implemented**). A later protocol version will replace this with
  a Noise handshake bound to the identities.
- A browser connecting by IP needs a TLS certificate valid for that IP.
- Connect is reached only over HTTPS (plain HTTP only on loopback), without
  following redirects (**implemented**). Servers also accept only a `200` reply
  (or `404` where absence is the answer), read at most 1 MiB and 1,000
  revocations per page, and give each request, body included, 20 seconds.

## Logging in to a server (vc/3, implemented)

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
   `--domain`, `--public-url`, and a global unicast `--public-ip` only when
   vc-server terminates TLS itself, with its listening port), never request headers. It checks that `expected_server_key` is its
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
shows `@handle` and the badge. A server cannot invent a handle for anyone. A
shown badge goes away when its attestation expires: the validity window is
checked again on every render, at expiry, every 30 seconds and whenever the
page is focused or visible again. After a clock change or sleep it stays only
until the next of these runs; a browser that delays or suspends the page's
timers delays that too.

Nicknames are the server's word. Next to a nickname that is not the handle
itself, the badge shows the verified `@handle` in full, so the check mark never
vouches for a name the server chose.

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

## Messages (implemented)

Every message, edit and attachment is signed by the author's session key:

```text
gwar message v2
{server_key}
{conversation}            server | channel:{id} | dm:{lower_uid}:{higher_uid}
{author_uid}
{client_signing_pk}
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
- Channels are named by id, and an honest server never reuses one (ids come
  from a counter that survives deletion). The client remembers, per server key,
  the channel ids it saw and saw deleted (by event or by their absence from a
  later channel list); messages in a channel whose id it saw deleted show "the
  server reused the id of a deleted channel" and never count as fully verified.
  The record fails closed: if it cannot be read or stored, holds anything but
  what the client wrote, or a server floods it past 10,000 ids, that server's
  record is marked incomplete for good and every channel message there is
  qualified ("cannot rule out a reused channel id"). If storage refuses even
  that small marker, the record stays in memory however many other servers
  the client visits. Memory holds 64 server records; once all 64 are such
  records, the next server the client loads makes it drop the oldest one, and
  from then until it reloads every channel message on every server is
  qualified. A reload is the one point where an unstored record is lost. A
  client that never saw the deletion cannot tell, so a signature binds a
  message to the channel id, not to the channel's name or lifetime. A direct
  message must involve exactly the two participants of the open conversation.
- Edits keep the message id and creation time and chain to the previous
  revision. A missing predecessor is shown as "continuity unproven"; two
  different revisions with the same number are a fork.
- The certificate is checked at `signed_at`. An expired or revoked device can
  still sign with a false time, so such messages show "signature valid; device
  revoked (or certificate expired); signing time unproven".
- IndexedDB checkpoints keep 10,000 signed identities atomically and refuse
  rollbacks, forks and predecessor substitution. Exact reloading is normal;
  fresh devices and evicted history have no checkpoints. Missing predecessors
  show continuity unproven.
- Clients fetch account-signed revocations from their own configured Connect,
  retaining account/device pairs permanently with the cursor. Feed omissions
  do not undo them. Stale/unavailable feeds are explicit; the 20,000-record
  capacity fails closed rather than evicting revocations.
- A client signs an edit or a deletion only over a revision of its own that
  verifies here; anything else goes to the server as an unsigned removal
  request. Every new message, edit, deletion and upload is bound to the
  connection it started on: if the client switches server, reconnects or
  resyncs before signing or sending, it is dropped, so nothing written for one
  server is signed for or sent to another. A failed message is kept for retry
  only while its session lasts, and is retried only on the server it was
  written for.
- Authenticity belongs to each received row, never to the server's numeric id.
  A row that verified is never displaced by one that did not under the same id
  or signed identity, and one signed identity is shown once.
- Author deletion is a final signed revision (the exact body encoding is in
  architecture.md). A verified deletion disappears, also on a device that never
  saw the earlier revisions; one whose signing device is revoked, expired or not
  checked against current revocations stays as an inspectable "deleted by the
  author" line with its warning. A moderator's unsigned removal
  says removed by the server.
- Until its signature is checked a message is visibly pending, so a quiet
  message is always a verified one.
- Notifications and mention highlights require current, continuous verification.
  For attachments that covers the signed metadata (name, size, hash); the bytes
  are hashed when they load, not before notifying. Attachments that fail or lack
  a signature load only on request. TeamSpeak messages are shown as unsigned.
- Images load automatically only near the visible part of the chat, at most
  three at a time and within an estimated 256 MiB of image memory (two RGBA
  frames plus the fetched bytes per image, reserved before decoding); the rest
  wait, and images far off screen are released.
- That estimate rests on the size read from the image header, so the header is
  read only where real decoders agree with it: a GIF frame must lie inside the
  logical screen (Chromium otherwise decodes the larger frame), a JPEG may have
  only one frame header before its scan (WebKit uses the last one), and a PNG
  must start with IHDR. Anything decoding larger than its header is still
  dropped after decoding. `apps/web/e2e/images.mjs` checks this against the
  Chromium and WebKit decoders; the Windows (WebView2) and Linux (WebKitGTK)
  desktop builds share those engines, but not necessarily their exact image
  libraries.
- Signature status, handle attestation, connection security and historical
  authorization are shown separately. A verified message does not mean the
  voice is authentic.

## Hostile input (implemented)

- The web app (also the desktop app's interface) validates every server message
  at runtime, with bounds, before use. The native protocol client in
  `vc-client`, used by tests and tools but not by the apps, bounds what a
  server can make it hold (frame size, requests awaiting a reply, announced
  clients). Its event stream passes on chat messages and edits only when their
  signature verifies for this server and conversation, or when they come
  unsigned from TeamSpeak; `Connection::verify_message` applies the same check
  to messages in replies such as history. It does not check device
  revocations or the continuity of edits, which need state the web app keeps.
- File URLs are confined to the server's own origin and to
  `/files/{id}/{name}` (GET) and `/api/files/{id}?token=` (PUT), in the web app
  and in the desktop app's native code.
- Unauthenticated connections are rate limited per IP and frames are size
  limited before parsing.
- The web app has a Content-Security-Policy. Gwar Connect marks every reply
  `Cache-Control: no-store`.

## What a malicious server can still do

These are limits of a voice chat relayed through a server:

- **Read everything** on that server: messages, direct messages, files, voice
  (the SFU terminates DTLS-SRTP). Direct messages are not end-to-end encrypted.
- **Lie about presence**: who is online, where, who is talking.
- **Voice**: inject audio or attribute it to anyone; voice is not signed.
- **Delivery**: drop, delay, reorder or hide messages. An observed server removal is
  distinct from a signed author deletion; silently omitted history cannot be detected
  on a fresh device.
- **Nicknames, channel names, roles** on that server are its word.
- **Show forged or altered content**, but only with a warning.
- **Track** your IP address, when you are online and how much you send, and
  link your uid across servers. Both are by design, not planned work: every
  service sees the address you connect from, and the uid is global so your
  identity, signatures and handle carry over between servers.

## What a malicious Connect can do

- Attest a handle to any key; it is the authority for handles. Clients remember
  the account behind each handle they verified and show a handle that moves to
  another account as changed, not verified (see "Handles"); a handle seen for
  the first time is taken as is.
- Withhold revocations: a revoked device stays accepted until its certificate
  expires (at most 90 days). Revocations already seen are kept forever.
- Run an offline guessing attack on your password with the stored blob; weak
  passwords fall, the recovery code (160 bits) does not.
- Serve an older vault (rollback), which can bring back a removed saved server
  or an old server password. A revoked device keeps the vault key it had.
- Deny service; learn which servers look up which accounts.
- It cannot sign as you, decrypt your account key without your password, read
  the vault, or forge revocations.

## Clients and the code supply chain

Every guarantee assumes your client runs genuine code.

- **Web app.** `gwar.app` is the only official web app. Whoever controls it
  or its TLS can serve modified JavaScript that reads your keys or password;
  web users trust the hosting on every load. It loads no third-party scripts,
  runs under a CSP, and `/version.json` names the commit it was built from, so
  anyone can compare it with the repository. That helps audits, not a user
  already served a modified page: a check run by code from the same origin
  proves nothing. Local identity keys are exportable so they can be backed up.
- **Desktop app.** Code is bundled; the webview has a CSP. Builds are not yet
  signed or notarized; there is no auto-updater. The native file commands
  confine requests to the file paths of the server origin the interface names.
  Each connection (every welcome, a resync too) gets a random file session;
  uploads, downloads and saves name the session they belong to, and native code
  refuses any other before choosing a destination or sending a byte, so work
  of a closed connection never reaches the next one, even on the same server.
  A compromised interface could still name any server, as it can connect to
  one.
- **Web apps hosted by other servers** are other origins: they don't have your
  keys, but they can phish your Connect password.
- **Uploaded files** are served with `nosniff`, a sandboxing CSP and
  `Content-Disposition`, from the server's origin. Attachments are hashed before
  display or download.
- **File links are capabilities by default.** A file's URL carries a random
  128-bit id and needs no login: anyone holding the link can download the file
  until its message is deleted, including people who were never on the server.
  Links posted to TeamSpeak (when the server has a public URL) are such links,
  as are links copied out of the app. Password channels protect their chat, not
  the links of files already shared.
- **Members-only files** (server setting `file_access: members`): a download
  also needs the bearer token of a live session that can read the file's
  channel, so a link alone, a former member or someone outside a password
  channel gets `404`. TeamSpeak users then get no links. The server itself
  still reads every file; this protects files from outsiders, not from the
  server.
- **Storage** is capped per member (`--upload-quota-mb`, 1 GiB) and in total
  (`--storage-limit-mb`, 10 GiB), counting files not yet sent. Identities are
  free, so the total cap is what protects the disk; uploads not attached to a
  message are deleted after a day. Space is freed only once a file's bytes are
  gone from disk, so a deletion that fails keeps counting until it is retried,
  and a file deleted during a download counts until that download ends (anyone
  holding the link can keep a deleted file counting that long).

## TeamSpeak

TeamSpeak users have no Gwar keys; their messages are marked as TeamSpeak and
never verified. TeamSpeak identities in the vault are shared by all of an
account's devices; revoking a device does not invalidate a copy it already has.

## Later

Noise for LAN connections, stronger evidence of signing time, vault
checkpoints against rollback, non-exportable signing keys, signed releases,
signed server key rotation, end-to-end encrypted direct messages.
