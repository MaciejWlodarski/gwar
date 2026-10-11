# vc/3 handshake implementation

Local implementation on `vc3-handshake`, verified on macOS ARM64, 2026-10-11.

## Wire formats

`challenge` contains `protocol: 3`, `server_pk`, `server_nonce`.
`hello` contains `protocol: 3`, `endpoint`, `expected_server_pk`, `server_nonce`,
`client_nonce`, `client_signing_pk`, optional `certificate`, `certificate_hash`,
`signature`. The hello reply is `{server_proof}`. Only then does the client send
`join {server_password?, invite?}`, whose successful reply is `Welcome`.
Application requests require a joined session. Hello rejects secret fields.

Keys and nonces are 32 bytes, signatures 64 bytes, hashes 32 bytes, encoded as
canonical unpadded base64url. Timestamps are nonnegative safe-integer Unix ms.
Transcript fields reject CR/LF. Statements are UTF-8 without a trailing newline.

`certificate_hash` is `-` for a local identity, otherwise:

```text
b64url(SHA256(UTF8("gwar device v1\n{account_pk}\n{device_pk}\n{issued_at}\n{expires_at}\n{account_signature}")))
```

Certificate wire names remain `account_key`, `device_key`, `issued_at`,
`expires_at`, `signature`. Client statement H:

```text
gwar client hello v3
{endpoint}
{expected_server_pk}
{server_nonce}
{client_nonce}
{client_signing_pk}
{certificate_hash}
```

`hello_hash = b64url(SHA256(UTF8(H)))`. Server statement:

```text
gwar server proof v3
{hello_hash}
{client_signature}
```

There are no statement-format deviations. Existing display metadata is retained:
challenge also carries `server` and `password_required`; hello also carries
`nickname` and `client`. These fields are outside H, preserving the connect UI
and initial member profile. No authorization or secret depends on their signature.

The web verifier additionally requires canonical non-weak prime-order key and
signature points. All seed-generated server keys and proofs meet this restriction.
It uses [`@noble/curves`](https://github.com/paulmillr/noble-curves#ecdsa-eddsa-schnorr-signatures) with ZIP-215 disabled. A direct local Chromium check
accepted an identity-point forgery that Node WebCrypto rejected, so WebCrypto
is not the handshake proof verifier. Regressions cover this difference.

Automatic IPv6 public-IP endpoints conservatively exclude special-purpose,
deprecated and unallocated ranges outside ordinary global unicast `2000::/3`.
Explicit `--public-endpoint` supports externally routed special-prefix endpoints.

## Configuration and state

- Added repeatable `--public-endpoint` / `VC_PUBLIC_ENDPOINTS`.
- Added `--insecure-any-host` / `VC_INSECURE_ANY_HOST`, with a loud warning.
- Removed `--public-host` / `VC_PUBLIC_HOSTS`.
- Added `vc-server --data-dir X server-key`, printing the canonical public key.
- Domain, public URL, explicit endpoints and explicitly configured global public
  IP are the only allowlist sources. Headers and inferred media addresses add none.
- Empty allowlists fail startup unless the explicit insecure flag is enabled.
- `server-key.ed25519` is a persistent 32-byte seed, atomically published with
  Unix mode 0600. The VM backup script copies it beside the Connect service key.
- Pins live in localStorage at `vc.server-pin:<normalized endpoint>`, storing
  `{server_pk, first_seen}`. A verified first proof commits the candidate; matches
  preserve the record. Mismatch blocks before signing or sending any data.
- Invite `server_key` is the full key and must match before signing. Invite
  mismatch cannot override trust. Pin mismatch displays both keys and offers an
  explicit dangerous forget-and-trust action; Cancel receives initial focus.
- English and Polish dialogs wrap fingerprints and buttons on mobile. Verified
  fingerprints appear in the sidebar. Desktop bookmarks persist `insecureLan`;
  only that desktop per-server toggle permits non-loopback WS. All connected
  plaintext sessions show the persistent red warning, including loopback.
- Web `Connection.serverKey` and native `Connection.server_key` expose the
  verified key. Native `expected_server_key` is optional only with explicit
  `trust_on_first_use`; its default is false. No legacy login fallback remains.
- Web certificate issuance and verification, and server handshake verification,
  now enforce the binding 90-day limit. Longer existing certificates need reissuance.

## Verification

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | Pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | Pass, no warnings |
| `cargo test --workspace` | 239 passed, 0 failed, 4 ignored |
| `cargo test -p vc-proto --features ts` | 48 passed, 0 failed |
| `pnpm install` | Pass, 248 packages reused |
| `pnpm --filter web add @noble/curves` | Pass, 2 packages installed |
| `pnpm --filter web test` | 411 passed, 0 failed, 37 files |
| `pnpm --filter web exec tsc --noEmit` | Pass |
| `pnpm --filter web build` | Pass |
| `cargo build -p vc-server -p gwar-connect` | Pass |
| `cargo test -p vc-server --test relay_binding` | 14 passed, 0 failed |

Final smoke command (free local ports, screenshots outside the checkout):

```sh
WEB_URL=http://127.0.0.1:59428 SERVER_HTTP=127.0.0.1:59429 E2E_SCREENSHOTS=/private/tmp/vc3-e2e-strict-screenshots pnpm --filter web e2e
```

Result: **61 passed, 0 failed**. Both services were rebuilt, then the web app was
rebuilt before this run.

The TypeScript generation and Rust/TS shared vectors passed: 11 accepted and
17 rejected endpoint inputs, plus local/device H and proof vectors.
The key CLI was read back twice: identical canonical public key, 32-byte file,
mode 0600. The removed host flag is rejected by Clap.
Screenshots were visually inspected for the fingerprint, warning and both
identity-change dialogs, including a 390 px Polish viewport.
Vite reports its existing large-chunk warning; the build succeeds.

Cargo's 239 passed count includes five native TeamSpeak fixture tests that
return early without `VC_TS3_DIR`. Their live bodies were not exercised.
Four server infrastructure tests remain explicitly ignored:
`puppet_nicknames_follow_members_and_resolve_collisions`,
`install_spawn_query_and_stop`, `query_against_running_server`,
`supervisor_restarts_killed_server`. No official TeamSpeak server ran on ARM64.

## Regression names

`crates/vc-server/tests/relay_binding.rs`:

- `accepted_endpoint_works_for_local_and_connect_identities`
- `relayed_signatures_for_another_endpoint_are_rejected_local_and_connect`
- `forged_endpoint_field_is_rejected_local_and_connect`
- `wrong_expected_server_key_and_replayed_connection_nonce_are_rejected`
- `proof_verifies_and_binds_every_transcript_field_and_client_signature`
- `join_and_application_requests_before_hello_are_rejected`
- `application_requests_after_proof_before_join_are_rejected`
- `hello_never_accepts_password_or_invite_fields`
- `strict_fields_reject_noncanonical_lengths_crlf_and_certificate_substitution`
- `empty_configuration_refuses_start_without_explicit_insecure_mode`
- `configured_global_public_ip_includes_self_terminated_tls_port_private_ips_do_not`
- `domains_public_url_and_explicit_endpoints_form_an_exclusive_set`
- `request_headers_cannot_add_endpoints`
- `invalid_public_endpoint_fails_startup`

`crates/vc-client/tests/handshake.rs`:

- `tofu_is_off_by_default_and_wrong_expected_key_sends_no_hello`
- `pinned_and_explicit_tofu_clients_verify_proof_before_sending_join_secrets`
- `missing_or_changed_transcript_proof_never_sends_join_secrets`

Web regressions in `net/handshake.test.ts`, `identity.test.ts`, `address.test.ts`
and `invite.test.ts` cover first use, match, mismatch before signing, invite
fingerprints, missing/wrong/tampered proof, fatal timeout/disconnect before proof,
proof before pin/join/password, cancellation during crypto, strict encodings,
weak points, exact shared statements, WS loopback/LAN policy and pin storage.
Smoke covers pin reuse, visible fingerprints, both blocking dialogs, explicit
trust, safe focus, mobile bounds, weak points in Chromium and keyed invites.

## Integration and limits

The new pre-proof `CoreHandle::device_authorized` gate mirrors the existing
revocation store lookup. The parallel Connect integrity change must adapt it
when revocations become keyed by `(account_key, device_key)`. Ongoing session
expiry and Connect service/account authorization remain that agent's scope.
The certificate-constant edit in `connect/crypto.ts` is the intentional small
merge overlap. Strict weak-point handling also matters for the parallel message
and Connect signature verifiers; native WebCrypto alone is not uniform.

The gateway emits proof before processing join; the clients enforce proof
verification before sending it. A server cannot observe whether a modified
client actually verified a proof. There is no additional client acknowledgement.

Validated TLS domain/IP aliases and the installed desktop LAN toggle were not
manually exercised on staging or a remote host. Local HTTPS tests passed; browser
smoke uses the authorized loopback exception. No push, deployment, remote-machine
operation, stash or other worktree change was performed.
