# Gwar Connect

Gwar Connect is the optional account service: log in on any device (web,
desktop, later mobile) and be the same person on every Gwar server. It is a
layer over Gwar's key-based identity, not a replacement:

- **Your identity is still a key.** An account owns an Ed25519 *account key*;
  servers derive your uid from it exactly as they always did. Using an existing
  local identity as the account key keeps your roles everywhere.
- **Connect never sees your password or your key.** The client derives two
  values from the password: an encryption key that never leaves the device, and
  an authentication key that Connect stores only as an Argon2 hash. The account
  key is stored encrypted. A leak of Connect's database lets nobody sign in as
  you anywhere.
- **Every device has its own key,** certified by the account key. A lost device
  is revoked without changing your identity.
- **Servers don't depend on Connect.** They verify device certificates
  themselves and only fetch the list of revoked devices (each entry signed by
  the account key) every few minutes. If Connect is down, servers keep working.

The official service runs at `https://gwar.maciejwlodarski.com/connect`. The
service is `crates/gwar-connect`.

## Encoding

All binary values are canonical base64url without padding. Timestamps are
unsigned decimal Unix milliseconds, within JavaScript safe integers. Strings
are UTF-8; passwords are NFKC-normalized first. Signed fields contain no CR/LF;
statements have no trailing newline.

## Keys and secrets

| Name | What | Where |
| --- | --- | --- |
| account key | Ed25519; uid = `b64url(sha256(public)[..20])` | private part only encrypted, in Connect |
| device key | Ed25519, one per device | private part only on the device |
| password secrets | `Argon2id(password, salt, m=65536 KiB, t=3, p=1, 64 bytes)` = `enc_key ‖ auth_key` (32 + 32 bytes) | derived on the device |
| recovery code | 20 random bytes, shown once as base32 in groups of 4 | with the user |
| recovery secrets | `HKDF-SHA256(ikm=code bytes, salt=account public key, info="gwar recovery v1", 64 bytes)` = `enc_key ‖ auth_key` | derived on the device |
| vault key | `HKDF-SHA256(ikm=account private seed, salt=account public key, info="gwar vault v1", 32 bytes)` | derived when the account key is decrypted; signed-in devices keep it |

The client-owned password profile is exactly Argon2id v=19, m=65536 KiB,
t=3, p=1, output=64 bytes, with a canonical 16–64-byte salt. The algorithm,
version and output length are implicit in `kdf`; only `{salt, m, t, p}` is
accepted. Clients reject other profiles before deriving or sending `auth_key`.
Connect accepts the same profile on registration and credential changes.

Connect stores `Argon2id(auth_key)` (its own salt, PHC string) for the password
and `sha256(recovery auth_key)` for the recovery code (160 bits of entropy).

**Key blob:** `b64url(nonce ‖ AES-256-GCM(enc_key, nonce, account private seed (32 bytes), aad))`
with a random 12-byte nonce and `aad = "gwar key v1\n" + account public key (b64url)`.
There is one blob for the password and one for the recovery code.

## Vault

Each account has one encrypted **vault**: a JSON object for secrets that have
to be the same on every device but are not Gwar keys. It holds the account
profile and TeamSpeak identities:

```json
{"profile": {"nickname": "gwar-48213", "updated_at": 1760000000000}, "teamspeak": {"identities": [{"uid": "<TeamSpeak uid>", "name": "Main", "identity": "<counter>V<obfuscated key>", "updated_at": 1760000000000}], "default": "<TeamSpeak uid>"}}
```

- `identity` is in the format of the official TeamSpeak client's identity
  export (the `identity="…"` value), so it can be imported from and exported to
  it; `uid` is its TeamSpeak unique id, stored so clients that cannot parse the
  key (the web app) can show it. `name` is a label the person chose.
  `updated_at` is when the entry was added or last changed.
- There is one entry per `uid`, and `default` is the `uid` used when nothing
  else is chosen. A reader repairs what it finds: it skips entries without a
  `uid` or `identity` and repeated `uid`s (the first stays), and a `default`
  that is not in the list means the first entry.
- **Older form.** Earlier clients wrote one identity,
  `{"teamspeak": {"identity": "…", "uid": "…", "updated_at": …}}`. Readers
  accept it as a list of one entry named "TeamSpeak" (also the name for an
  entry that has none), but only when `identities` is missing or has no usable
  entry. Writers write only the list form and drop the three older fields.
- **Encrypted with the vault key:** `b64url(nonce ‖ AES-256-GCM(vault_key, nonce, UTF-8 JSON, aad))`,
  random 12-byte nonce, `aad = "gwar vault v1\n" + account public key (b64url)`.
  The key comes from the account key, so changing the password or recovering
  the account changes nothing here, and Connect only ever sees ciphertext.
- Signed-in devices keep the vault key (it cannot sign anything), so they read
  and update the vault without asking for the password.
- Clients keep fields they do not know when they write the vault back,
  including unknown fields of `teamspeak` and of the entries that stay.
  Writes name the version they started from; on `409 conflict` fetch, merge and
  retry.

TeamSpeak has no notion of devices: every device signed in to the account uses
the same TeamSpeak key. Revoking a device stops it from fetching the vault,
but a copy it already has stays valid on TeamSpeak servers; replace the
TeamSpeak identity if that matters.

The `profile` section holds the account identity's global `nickname` (trimmed,
non-empty, at most 32 characters) and `updated_at` (Unix milliseconds). It is
used when joining a server for the first time; existing members keep their
server nickname. After unlocking, clients adopt the vault nickname or seed
an absent profile from this device's local identity nickname. At startup they
refresh it from the vault. Edits write the encrypted vault first with its
optimistic version, re-read and retry on `409 conflict`, then update the local
cache. Writers preserve other profile fields and all other vault sections.

## Signed statements

All signatures are Ed25519 over the UTF-8 text, fields separated by `\n`:

- **Device certificate**, by the account key:
  `gwar device v1\n{account_key}\n{device_key}\n{issued_at}\n{expires_at}`.
  Certificates last at most 90 days. Whenever a client has the account key
  (sign-up, log-in, recovery, password change, revocation), it certifies only
  the public key derived from the private device key it generated and holds.
  Each device renews its own certificate after entering the password. The
  device inventory is for inspection and revocation; its names, last-seen
  times and completeness are unverified service claims, never instructions
  to sign. A device close to expiry asks for the password.
- **Revocation**, by the account key: `gwar revoke v1\n{account_key}\n{device_key}\n{revoked_at}`.

### Session and credential challenges

`service_key` is the Ed25519 public key returned by the configured service's
`GET /v1/service-key`. The client obtains it itself, then checks that the
challenge names that same key. `token_hash` is
`b64url(SHA256(UTF8(enrollment-or-session-token)))`.

A challenge is 32 fresh random bytes, expires after five minutes, and is bound
in SQLite to the account, token, purpose and expected credential version.
Only one challenge per token and purpose stays outstanding. Each successful
operation consumes it once, atomically with the session or credential update.
Clients reject malformed, expired or more-than-five-minute challenges.

**Session proof**, by the locally held device key:

```text
gwar session v1
{service_key}
{account_key}
{certificate_hash}
{token_hash}
{challenge}
{challenge_expires_at}
```

`certificate_hash` is `b64url(SHA256(UTF8(device_statement + "\n" +
account_signature)))`: all certificate fields and the account signature.
The purpose is session issuance, fixed by the statement prefix. The service
checks the account certificate, device ownership, revocation, expiry and fresh
device signature. It consumes the enrollment token too. No persistent token
is issued by registration, password login or recovery alone.

**Credential update**, by the account key:

```text
gwar credentials v1
{operation}
{service_key}
{account_key}
{token_hash}
{challenge}
{challenge_expires_at}
{credential_version}
{kdf_hash}
{auth_key_hash}
{key_blob_hash}
```

`operation` is `password` or `recovery`. For a password, `kdf_hash` is
`b64url(SHA256(UTF8("gwar kdf v1\nargon2id\n19\n{salt}\n{m}\n{t}\n{p}\n64")))`.
`auth_key_hash` and `key_blob_hash` hash the decoded complete new `auth_key`
and `key_blob` bytes, including the blob nonce and tag. For recovery,
`kdf_hash` is the literal `-`, and the other hashes cover `recovery_auth` and
`recovery_blob`. The expected version starts at 0 and rises by one on either
update. Both changes end other tokens. The current token stays device-bound.
A bearer token alone cannot change credentials. After signature verification,
the transaction rechecks current session authorization, the challenge and the
expected version before committing; a race or replay cannot commit twice.

## Connect API

JSON over HTTPS under `/v1`. Authenticated calls send
`Authorization: Bearer <token>`. Enrollment tokens expire after five minutes,
without sliding renewal. Persistent sessions end after 90 days without use or
at the expiry of the certificate proven when issued, whichever comes first. Errors are
`{"error": "<code>", "message": "..."}` with an HTTP status.

| Call | Body → reply |
| --- | --- |
| `POST /v1/register` | `{handle, account_key, kdf: {salt, m, t, p}, auth_key, key_blob, recovery_auth, recovery_blob, device: {device_key, name, issued_at, expires_at, signature}}` → `{token}` (enrollment only, bound to this device). The account certificate is checked; a fresh device proof is still required. |
| `POST /v1/prelogin` | `{handle}` → `{kdf}` (a stable fake for unknown handles) |
| `POST /v1/login` | `{handle, auth_key}` → `{token, account_key, key_blob}` (enrollment only) |
| `POST /v1/recover` | `{handle, recovery_auth}` → `{token, account_key, recovery_blob}` (enrollment only) |
| `GET /v1/account` | → `{handle, account_key, created_at}` |
| `POST /v1/challenges` | `{purpose: "session" \| "password" \| "recovery", device?: certificate}` → `{challenge, expires_at, credential_version, service_key}`. Session purpose takes an enrollment token; credential purposes take a device session. |
| `POST /v1/session` | `{challenge, device: certificate, signature}` → `{token}` (device session); `signature` is the device proof above. |
| `PUT /v1/account/password` | `{kdf, auth_key, key_blob, challenge, credential_version, signature}` → `{}`; account-signed credential update. |
| `PUT /v1/account/recovery` | `{recovery_auth, recovery_blob, challenge, credential_version, signature}` → `{}`; account-signed credential update. |
| `GET /v1/devices` | → `{devices: [{device_key, name, created_at, last_seen, revoked_at, certificate: {issued_at, expires_at, signature} \| null}]}` (inventory metadata is unverified; certificate signatures are checked locally) |
| `POST /v1/devices` | `{device_key, name, issued_at, expires_at, signature}` → `{}`; enrollment token only, bound to the first device it enrolls or renews. Revoked devices stay revoked. |
| `POST /v1/devices/revoke` | `{device_key, revoked_at, signature}` → `{}` (that device's sessions end) |
| `GET /v1/vault` | → `{vault, version, updated_at}` (`vault: null, version: 0` before the first write) |
| `PUT /v1/vault` | `{vault, version}` (the version the change was based on) → `{version}`; `409 conflict` if it changed meanwhile. At most 64 KiB. |
| `POST /v1/logout` | → `{}` |
| `GET /v1/revocations?since=<seq>` | public → `{revocations: [{seq, account_key, device_key, revoked_at, signature}]}` (at most 1000, ascending) |
| `GET /v1/accounts/{handle}` | public → `{handle, account_key}`; `404 not_found` if unknown |
| `GET /v1/accounts/by-key/{account_key}` | public → `{handle, account_key}`; `404 not_found` if unknown |

`POST /v1/devices/renew` is removed. Each client certifies only its own locally
held key through `POST /v1/devices`, then proves possession for a new session.
Revoking that account's device ends every token bound to it, including pending
enrollment credentials. Old sessions do not gain lifetime from a later
certificate renewal; the device obtains a fresh session too.

Handles are 3–32 characters of `a-z 0-9 _ .`, compared lowercase. Registration,
login and recovery are rate-limited per address and per handle.

## On Gwar servers

The vc/3 `hello` has an optional `certificate` field:
`{account_key, device_key, issued_at, expires_at, signature}`. With it,
`client_signing_pk` must equal `device_key`; the uid comes from `account_key`.
The server checks the certificate's signature, current validity and revocation
before sending its identity proof, and checks authorization again at join.
Certificates last at most 90 days. Without a certificate, the local signing
key remains the identity. Passwords and bearer invite codes are sent in `join`
only after the client verifies the server proof; they are rejected in hello.

All binary fields use strict canonical unpadded base64url. Public keys and
nonces decode to exactly 32 bytes, signatures to 64, hashes to 32. Fields may
not contain CR/LF; timestamps are unsigned decimal Unix milliseconds in the
JavaScript safe-integer range. Statements are UTF-8 with **no trailing newline**.
Both sides generate independent random 32-byte nonces for every connection.

`certificate_hash` is literal `-` for a local identity, otherwise
`b64url(SHA256(UTF8("gwar device v1\n{account_pk}\n{device_pk}\n{issued_at}\n{expires_at}\n{account_signature}")))`.
Certificate wire fields are `account_key`, `device_key`, `issued_at`,
`expires_at`, `signature`; the commitment includes that account signature.

Client statement H, signed by `client_signing_pk`:

```text
gwar client hello v3
{endpoint}
{expected_server_pk}
{server_nonce}
{client_nonce}
{client_signing_pk}
{certificate_hash}
```

`hello_hash = b64url(SHA256(UTF8(H)))`. The server signs:

```text
gwar server proof v3
{hello_hash}
{client_signature}
```

`client_signature` is the exact canonical `hello.signature` value.

Servers fetch `GET {connect}/v1/revocations?since=…` every five minutes
(`--connect-url`, default the official service; `--no-connect` turns it off),
verify each entry with its account key and permanently retain it under the
pair `(account_key, device_key)`. Lookups and live-session eviction use that
same pair. Sequence numbers only advance: entries at or before the current
cursor are ignored, and a missing entry never clears a stored revocation.
Connect can withhold the feed; its sequence numbers do not prove completeness.
The core disconnects an ongoing session at certificate expiry with an explicit
renewal reason, removes its subscriptions and voice routing, and closes its
SFU transport. The 400-day certificate limit is unchanged.

A certificate proves key possession, not registration with Connect. After a
device-authenticated hello, servers asynchronously look up the account key via
`/v1/accounts/by-key/{account_key}` and cache the signed handle attestation and next-check
time in `users`. Before HTTP, a five-minute retry guard is persisted across devices
and restarts; a valid 200 extends it to 24 hours, a 404 to one hour, and errors keep
the five-minute guard. A refreshed attestation emits `member.updated`;
404 clears it, while an outage keeps the cached value. A hello signed directly
with the key (no device certificate, e.g. an old exported root backup) keeps a
confirmed handle and refreshes it the same way, since the
handle belongs to the key. Keys never confirmed are not looked up then, so people
who don't use Connect are never reported to it. `--no-connect` clears all stored attestations
and next-check times on startup; certificates still authenticate locally. Both public
account lookups have the same policy (no login rate limit). The literal `by-key`
cannot be a handle because hyphens are not allowed.

## What your client verifies

Your client checks each handle's Ed25519 attestation against the service key it
fetches from its configured Connect service over HTTPS. It checks the signed
account key against the member's uid and rejects expired attestations or issuance
more than five minutes in the future.
The game server can still lie about presence (who is online or in which channel),
voice, and, until message signing lands, message authorship.

## Service key and handle attestations

Connect keeps its service signing seed in `service-key.ed25519` in the data
directory (0600 on Unix). Back it up with the database. `gwar-connect --data-dir <dir> service-key` prints the public key; `GET /v1/service-key` returns
`{"key":"<base64url public key>"}`. Both public account lookups include a fresh
seven-day `attestation` with `account_key`, `handle`, `issued_at`, `expires_at`
(milliseconds), and `signature`. The signed UTF-8 bytes, with no trailing newline:

```text
gwar handle v1
{account_key}
{handle}
{issued_at}
{expires_at}
```

Clients refresh the key once per app session and cache it by Connect URL in memory
and localStorage. A previously fetched key can be used during an outage; without
a key, no handles or Connect badges appear. Local development permits loopback
HTTP. Servers cache the entire attestation and omit it once expired.

## Client flows

- **Sign up:** choose a handle and password; use the current identity as the
  account key (keeps roles) or a new one; make a device key and certificate;
  show the recovery code once. On activation, durably store the device record
  and remove a matching plaintext local root in one IndexedDB transaction.
  Older signed-in records are repaired on load and sign-out. An unrelated
  local identity stays; otherwise sign-out creates a fresh local identity.
  A previously exported identity backup remains a full account credential,
  even after revoking the device that exported it.
- **Log in on a device:** prelogin → derive secrets → login → decrypt the
  account key in memory → certify a fresh local device key → register the device
  with the enrollment credential → challenge → device proof → device session →
  forget the account key. The device keeps only its own key and certificate.
- **Revoke a device / change password:** needs the password again (to decrypt
  the account key for the signature or re-encrypt it).
- **Lost password:** the recovery code decrypts the recovery blob; then set a
  new device session, then set a new password with the account signature.
- **TeamSpeak identities (desktop):** while signed in, TeamSpeak connections
  use the account's identities from the vault (the default one, or the one
  chosen for a connection); signing out returns to the device's own list. After
  a flow that has the vault key: if the vault has identities, the desktop adopts
  them as its account list; otherwise the first desktop seeds the vault with its
  whole device list (making one identity named "Default" first if it has none),
  keeping its TeamSpeak groups. Desktops refresh from the vault when they start,
  quietly. Every change (add, import, rename, delete, set default, generate) is
  written to the vault first and then to the desktop; signed out it only changes
  the device's list. Deleting the default makes the first remaining one the
  default. The browser shows the list read-only.
  The desktop keeps the lists as `teamspeak-identities.json` (device) and
  `teamspeak-identities.account.json` (account, removed on sign-out), each
  `{"default": "<uid>", "identities": [{"uid", "name", "identity"}]}`. A single
  identity file of older versions (`teamspeak-identity.json`, and
  `teamspeak-identity.account.json`) becomes a list of one named "Default" the
  first time it is read; the old file is left in place.
- **Finding identities from the official TeamSpeak client (desktop):** only when
  the user asks (a button, or accepting the prompt before the first TeamSpeak
  connection on a device, asked once), the desktop reads the client's
  `settings.db` on this computer, read-only (or, if that fails, from a copy
  made in a private temporary directory and deleted right after). It
  understands TeamSpeak 3.6+ (table `ProtobufItems`; the last used identity is
  in `Connecting`) and the older INI-like format; TeamSpeak 6 is tried with the
  same parsers. The user chooses which identities to add. Those below security
  level 8 are listed but cannot be chosen.

## Migration

Connect's third SQLite migration preserves accounts, encrypted blobs, devices,
revocations and vaults, adds `credential_version = 0` and challenge storage,
and invalidates all old tokens: their device possession was never proven.
Sign in or use the recovery code to obtain a new session. Old non-allowlisted
password KDFs cannot be upgraded without the password; these clients must use
recovery to set the supported profile. Existing valid-profile accounts need
no credential migration.

The server migration changes the revocation primary key to the account/device
pair without losing rows. New rows retain the account signature; legacy rows
keep their recorded pair and time, with no stored signature. The feed cursor
and signed revocations are persisted together. Nothing deletes revocations.

Client activation saves the device and deletes a matching plaintext root in
one strict-durability IndexedDB transaction. Failure leaves both records as
before. Legacy signed-in records are repaired on load or sign-out. A legacy
browser already signed out has no account record identifying its root; sign
in again to migrate it. Exported root backups cannot be removed by migration.

Vault encryption authenticates contents, not freshness: a malicious service
can replay an old vault and version. Revocation prevents future honest-service
access, but cannot erase retained vault keys, copied plaintext or TeamSpeak
credentials. Vault checkpoints and root-backup revocation are separate work.

## Test vectors

For checking a client implementation (all values base64url; the same as
`client_crypto_vectors` in `crates/gwar-connect/tests/api.rs`):

| Input | Output |
| --- | --- |
| password `correct horse`, salt = 16 × `0x07`, m = 65536, t = 3, p = 1 | `enc_key` = `qXcjpHbTOr46vWBB7wO-b3l-lYMp7lsl8sa_SekxVr0`, `auth_key` = `wWSVGXCU8xhR5oEnsTNapLPwB1BEFO42MQySTQK-LK0` |
| account seed = 32 × `0x01` | account key = `iojj3XQJ8ZX9UtstPLpdcspnCb8dlBIb83SIAbQPb1w` |
| recovery code = 20 × `0x09`, that account key | `enc_key` = `WA19s_g5IcLVVJ_xQ3KHzTpUn7mkdbLdTKOA3GCok10`, `auth_key` = `42kqZaRu_Jtg8r8S7P-39L9-QR5IQM8eSwza09oTy3M` |
| key blob of that seed under the password `enc_key`, nonce = 12 × `0x03` | `AwMDAwMDAwMDAwMDEz57zhVdpeE9DuPc-Z8DMvCEty7oHW-BIq7g3-5P-1LKErFt1chgOHB2VhOzkcXy` |
| account seed = 32 × `0x01` | vault key = `_LvSNss8p0PDJFYkx77Gv9dldHZztz1NPse-h8ZV6bQ` |
| vault `{"teamspeak":{"identity":"1V","uid":"u","updated_at":1}}` (exactly these bytes) under that key, nonce = 12 × `0x03` | `AwMDAwMDAwMDAwMDZgbHGQNI33i1QJYlr7AP1RbMIJv2WqEwRVph4x0WE_aS19RAscHc07sG1nH6et8NwocuVgdgFXyTeXCsQHpTYOstEa3cT7_Q` |
| device seed = 32 × `0x02` (key `gTl3Dqh9F19Wo1Rmw0x-zMuNipG07jeiXfYPW4_Js5Q`), issued 1, expires 2 | certificate signature `hrl_m1SlEOKgUYV7RMz4dnlZ5wa5OY_m5-ZS9qWcZU82oGdpwkQdm5wVyUgxplRzxEiMoJnAs-BN6ktG0ufaDA` |


The session and credential vectors are shared with the web tests in
`crates/gwar-connect/tests/integrity-vectors.json` (`client_integrity_vectors`).
They use the account/device seeds above, service seed = 32 × `0x2a`, token
`token`, challenge = 32 × `0x03`, challenge expiry 5, credential version 0,
salt = 16 × `0x07`, new auth key = 32 × `0x04`, blob = 60 × `0x05`.
The small timestamps exercise encoding only.

| Statement | SHA256 (base64url) | Signature (base64url) |
| --- | --- | --- |
| session, by device seed `0x02` | `6rMfx1f27_bJrlyv2I7eKcac4johY4RJFCKX5eJrWyo` | `8iVGzkPVxFA4GiZAE3xipo1dcm13gTuqfUlB2_Kw3Lbb6-YtNGT-1gYf34aPkksG7IIiMx6O9AB-uiHHUpEGBg` |
| password, by account seed `0x01` | `u_Ms42GSdCxYSNR4nc35_cgUh7m6KwPcU53A1PN7SeE` | `jTQPOtyVGVj706dX1wJHcOQ3ep3AQ-vIQTtZqlXmYg5O3a_gzwAKJ4_Zt7mW_kxIG9ed8s9Jkf9gr0_ONfblCA` |
| recovery, by account seed `0x01` | `7nj2kTwERRSf-dfUHhKKQ7sIWEc5S98QjA5SpPLhUo4` | `xZ6lZ9xsC6ziHblOtbw_aBEKbVpVlOsFjhclfyJ_QWxp9g1sS3e807quydV_7CGdQGgX-5Q_lO56VpKeoYXDBw` |
