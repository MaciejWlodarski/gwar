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

All binary values are base64url without padding. Timestamps are Unix
milliseconds. Strings are UTF-8; passwords are NFKC-normalized first.

## Keys and secrets

| Name | What | Where |
| --- | --- | --- |
| account key | Ed25519; uid = `b64url(sha256(public)[..20])` | private part only encrypted, in Connect |
| device key | Ed25519, one per device | private part only on the device |
| password secrets | `Argon2id(password, salt, m=65536 KiB, t=3, p=1, 64 bytes)` = `enc_key ‖ auth_key` (32 + 32 bytes) | derived on the device |
| recovery code | 20 random bytes, shown once as base32 in groups of 4 | with the user |
| recovery secrets | `HKDF-SHA256(ikm=code bytes, salt=account public key, info="gwar recovery v1", 64 bytes)` = `enc_key ‖ auth_key` | derived on the device |
| vault key | `HKDF-SHA256(ikm=account private seed, salt=account public key, info="gwar vault v1", 32 bytes)` | derived when the account key is decrypted; signed-in devices keep it |

Connect stores `Argon2id(auth_key)` (its own salt, PHC string) for the password
and `sha256(recovery auth_key)` for the recovery code (160 bits of entropy).

**Key blob:** `b64url(nonce ‖ AES-256-GCM(enc_key, nonce, account private seed (32 bytes), aad))`
with a random 12-byte nonce and `aad = "gwar key v1\n" + account public key (b64url)`.
There is one blob for the password and one for the recovery code.

## Vault

Each account has one encrypted **vault**: a JSON object for secrets that have
to be the same on every device but are not Gwar keys. Today it holds the
TeamSpeak identity:

```json
{"teamspeak": {"identity": "<counter>V<obfuscated key>", "uid": "<TeamSpeak uid>", "updated_at": 1760000000000}}
```

- `identity` is in the format of the official TeamSpeak client's identity
  export (the `identity="…"` value), so it can be imported from and exported to
  it; `uid` is its TeamSpeak unique id, stored so clients that cannot parse the
  key (the web app) can show it.
- **Encrypted with the vault key:** `b64url(nonce ‖ AES-256-GCM(vault_key, nonce, UTF-8 JSON, aad))`,
  random 12-byte nonce, `aad = "gwar vault v1\n" + account public key (b64url)`.
  The key comes from the account key, so changing the password or recovering
  the account changes nothing here, and Connect only ever sees ciphertext.
- Signed-in devices keep the vault key (it cannot sign anything), so they read
  and update the vault without asking for the password.
- Clients keep fields they do not know when they write the vault back.
  Writes name the version they started from; on `409 conflict` fetch, merge and
  retry.

TeamSpeak has no notion of devices: every device signed in to the account uses
the same TeamSpeak key. Revoking a device stops it from fetching the vault,
but a copy it already has stays valid on TeamSpeak servers; replace the
TeamSpeak identity if that matters.

## Signed statements

All signatures are Ed25519 over the UTF-8 text, fields separated by `\n`:

- **Device certificate**, by the account key:
  `gwar device v1\n{account_key}\n{device_key}\n{issued_at}\n{expires_at}`.
  Certificates last at most 400 days; clients renew them at login.
- **Revocation**, by the account key: `gwar revoke v1\n{account_key}\n{device_key}\n{revoked_at}`.

## Connect API

JSON over HTTPS under `/v1`. Authenticated calls send
`Authorization: Bearer <token>` (sessions last 90 days). Errors are
`{"error": "<code>", "message": "..."}` with an HTTP status.

| Call | Body → reply |
| --- | --- |
| `POST /v1/register` | `{handle, account_key, kdf: {salt, m, t, p}, auth_key, key_blob, recovery_auth, recovery_blob, device: {device_key, name, issued_at, expires_at, signature}}` → `{token}`. The device certificate proves possession of the account key. |
| `POST /v1/prelogin` | `{handle}` → `{kdf}` (a stable fake for unknown handles) |
| `POST /v1/login` | `{handle, auth_key}` → `{token, account_key, key_blob}` |
| `POST /v1/recover` | `{handle, recovery_auth}` → `{token, account_key, recovery_blob}` |
| `GET /v1/account` | → `{handle, account_key, created_at}` |
| `PUT /v1/account/password` | `{kdf, auth_key, key_blob}` → `{}` (other sessions end) |
| `GET /v1/devices` | → `{devices: [{device_key, name, created_at, last_seen, revoked_at}]}` |
| `POST /v1/devices` | `{device_key, name, issued_at, expires_at, signature}` → `{}` |
| `POST /v1/devices/revoke` | `{device_key, revoked_at, signature}` → `{}` (that device's sessions end) |
| `GET /v1/vault` | → `{vault, version, updated_at}` (`vault: null, version: 0` before the first write) |
| `PUT /v1/vault` | `{vault, version}` (the version the change was based on) → `{version}`; `409 conflict` if it changed meanwhile. At most 64 KiB. |
| `POST /v1/logout` | → `{}` |
| `GET /v1/revocations?since=<seq>` | public → `{revocations: [{seq, account_key, device_key, revoked_at, signature}]}` (at most 1000, ascending) |
| `GET /v1/accounts/{handle}` | public → `{handle, account_key}` |

A session belongs to the device it registered (`register`, `POST /v1/devices`),
so revoking a device signs it out.

Handles are 3–32 characters of `a-z 0-9 _ .`, compared lowercase. Registration,
login and recovery are rate-limited per address and per handle.

## On Gwar servers

`hello` gains an optional `device` field:
`{account_key, device_key, issued_at, expires_at, signature}`. With it,
`public_key` must equal `device_key` and sign the challenge as before; the
server checks the certificate (signature, validity, not revoked) and the uid
comes from `account_key`. Without it nothing changes, so local identities keep
working.

Servers fetch `GET {connect}/v1/revocations?since=…` every five minutes
(`--connect-url`, default the official service; `--no-connect` turns it off),
verify each entry with its account key and refuse revoked devices.

## Client flows

- **Sign up:** choose a handle and password; use the current identity as the
  account key (keeps roles) or a new one; make a device key and certificate;
  show the recovery code once.
- **Log in on a device:** prelogin → derive secrets → login → decrypt the
  account key in memory → certify a fresh device key → register the device →
  forget the account key. The device keeps only its own key and certificate.
- **Revoke a device / change password:** needs the password again (to decrypt
  the account key for the signature or re-encrypt it).
- **Lost password:** the recovery code decrypts the recovery blob; then set a
  new password.
- **TeamSpeak identity (desktop):** while signed in, TeamSpeak connections use
  the account's TeamSpeak identity from the vault; signing out returns to the
  device's own one. The first desktop to sign in to an account without one
  stores its own (keeping its TeamSpeak groups); the user can also import one
  exported from the official TeamSpeak client. Desktops refresh it from the
  vault when they start.
  The account's identity is kept next to the device's own
  (`teamspeak-identity.account.json` beside `teamspeak-identity.json`) and removed on sign-out.

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
