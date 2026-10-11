# Connect integrity implementation

Branch: `connect-integrity`. Worktree: `/Users/zireael/dev/projects/gwar-connect-integrity`.
No push, deployment or remote machine access.

## Changes and regressions

1. **Signing oracle.** Certificate issuance derives the device public key from a private key held locally. Sign-up and sign-in generate that key locally; password change, revocation, vault unlock and renewal derive the existing device key from its local seed. None reads the inventory to decide what to certify. The inventory remains available for inspection and revocation, with English/Polish text identifying names, activity and completeness as service claims. `POST /v1/devices/renew` is removed. The existing `gwar device v1` format is unchanged.
   - Web: `never certifies an attacker key inserted in the inventory in any unlocking flow`; `derives the renewal public key from the locally held private key`.
   - Connect API: `bulk_renewal_is_removed`; `enrollment_tokens_cannot_access_the_vault_or_change_credentials` also rejects enrolling a second device with a bound enrollment credential.

2. **KDF downgrade.** The sole client profile is Argon2id v=19, m=65536 KiB, t=3, p=1, 64 bytes. The wire format remains `{salt, m, t, p}`; algorithm, version and length are fixed. Salt must be canonical unpadded base64url representing 16–64 bytes. Extra fields and alternate profiles are rejected before WASM derivation or `auth_key` transmission. Encoded lengths are bounded before decoding. Connect enforces the same profile on registration and password updates and rejects unknown KDF fields.
   - Web: `rejects downgrades, resource exhaustion and alternate profiles before hashing`; `rejects a malicious prelogin profile before sending auth_key`.
   - Connect API: `registration_rejects_unsupported_password_profiles`; `credential_updates_require_the_account_signature_over_every_new_field` includes a downgraded password-update KDF.

3. **Plaintext root migration.** Activation commits the device record before profile/network work. Its IndexedDB write stores the device and removes a matching `identity` root in one transaction with strict durability, resolving only at transaction completion. Unrelated local identities remain. Legacy signed-in records are repaired on load and sign-out, preserving the nickname. Sign-out creates a fresh local identity if the previous one became the account root. The keep-identity hint in English and Polish explains that old exported backups remain full account credentials. The device-less server handle-refresh behavior remains compatible.
   - Web, `durable root migration`: `saves the device and removes the root atomically; sign-out creates a different identity`; `an aborted enrollment preserves the root and stores no device`; `keeps an unrelated local identity`; `repairs a legacy signed-in root before returning the active identity, preserving its nickname`; `sign-out repairs legacy root storage even when the identity was not loaded`.
   - Real Connect/browser e2e: `connect: a new account keeps the local identity and shows the recovery code once` reads IndexedDB to reject a retained root; `connect: signing out never restores the account root; the recovery code signs in again` rejects restoration of its uid.
   - Server: `an_account_key_without_a_device_keeps_its_handle` remains passing.

4. **Device-bound sessions.** Register, password login and recovery issue five-minute, non-sliding enrollment credentials. They can enroll or renew their first device, request a session challenge and exchange its proof for a persistent token. They cannot read/write the vault, list devices, revoke devices or update credentials. Every persistent issuance consumes a random 32-byte, five-minute, one-use challenge and its enrollment credential. The device must have a valid account certificate and remain non-revoked. Every account flow uses this exchange. Revocation ends all bound tokens. Session use cannot extend expiry beyond the certificate proven at issuance.
   - New endpoints: `POST /v1/challenges`, `POST /v1/session`.
   - Device-signed statement, UTF-8, no trailing newline: `gwar session v1\n{service_key}\n{account_key}\n{certificate_hash}\n{token_hash}\n{challenge}\n{challenge_expires_at}`. The prefix fixes the session-issuance purpose. The certificate hash includes the complete certificate statement and account signature; the token hash binds the enrollment credential. The service key is fetched from the configured service and checked against the challenge.
   - Web: `password change, revocation and vault unlock keep tokens bound to this device and revocation ends them`; `refuses a substituted or expired challenge before producing the device proof`.
   - Connect API: `enrollment_tokens_cannot_access_the_vault_or_change_credentials`; `session_proofs_are_one_use_and_bind_service_account_certificate_and_token`; `expired_challenges_and_device_certificates_end_connect_access`; `revoking_a_device_ends_its_sessions`.

5. **Credential-update authorization.** Password and recovery credential updates require the account key. The signature covers operation, service, account, authorizing session, fresh challenge and expiry, expected credential version and hashes of every new credential field. After verification, the SQLite transaction rechecks session authorization, version and challenge expiry/consumption, updates the credentials/version and ends other tokens together. A concurrent request or replay cannot commit twice. Recovery first obtains a device session, then signs its password change. Recovery-code rotation has the same authorization protocol.
   - Endpoints: `PUT /v1/account/password`, `PUT /v1/account/recovery`.
   - Account-signed statement, UTF-8, no trailing newline: `gwar credentials v1\n{operation}\n{service_key}\n{account_key}\n{token_hash}\n{challenge}\n{challenge_expires_at}\n{credential_version}\n{kdf_hash}\n{auth_key_hash}\n{key_blob_hash}`.
   - Operations: `password`, `recovery`. KDF hash input: `gwar kdf v1\nargon2id\n19\n{salt}\n{m}\n{t}\n{p}\n64`. Authentication and blob hashes cover their complete decoded bytes. Recovery uses literal `-` for the KDF hash and hashes the new recovery authentication key/blob instead. Versions start at 0 and increment on either update.
   - Web: `rejects alteration of the KDF, authentication key or key blob after the account signed them`; `rotates recovery credentials with an account-signed update`.
   - Connect API: `credential_updates_require_the_account_signature_over_every_new_field`; `recovery_credential_updates_are_signed_and_versioned_too`; `the_recovery_code_restores_access_and_a_new_password`.
   - Shared Rust/TypeScript vectors: `client_integrity_vectors`; `matches Rust session and credential statements, hashes and signatures`, backed by `crates/gwar-connect/tests/integrity-vectors.json`. `docs/connect.md` contains the statement/hash/signature vectors.

6. **Cross-account revocations.** Revocations are permanently stored and looked up by `(account_key, device_key)`; eviction matches both. New rows retain the account signature. Feed processing sorts entries, ignores old/duplicate sequence numbers and never lowers the cursor. Revocations and cursor advance commit together. An empty feed does not erase revocations. The `gwar revoke v1` statement and public feed JSON remain unchanged.
   - Server: `cross_account_revocations_do_not_poison_login_or_live_sessions` covers account X revoking a device certified by Y, unaffected Y login/live sessions, old sequence rejection, valid Y revocation, omitted entries and restart persistence; `revocation_cursors_ignore_old_and_duplicate_entries_and_never_go_backwards`.

7. **Live certificate expiry.** The core schedules the earliest certificate expiry and also expires sessions before processing messages, including deferred operations. It disconnects the device with an explicit renewal reason, removes subscriptions and voice routing, and queues SFU peer removal even when the media queue is full. Later commands cannot revive the session. Local identities have no device deadline. No wire change; the existing kicked/disconnected reason is used. The 400-day limit and 365-day client issuance remain unchanged.
   - Server: `certificate_expiry_disconnects_an_idle_session_and_ends_voice_routing` presents a real short-lived certificate and checks disconnect, reason, routing removal and absence from later snapshots; `expiry_queues_sfu_removal_even_when_the_media_queue_is_full` checks guaranteed removal delivery and rejects late participation.

## Migrations

- Connect SQLite migration 3 preserves accounts, devices, encrypted blobs, revocations and vaults; adds credential version 0, session scope/certificate expiry and challenge storage; invalidates every legacy token because no fresh device proof backed it. Reauthentication is required.
- Existing supported-profile credentials remain usable. A non-allowlisted legacy KDF cannot be transformed without the password; use recovery to install the supported profile. Data is retained.
- The vc-server migration replaces the device-only revocation primary key with the account/device pair, preserving all rows. Legacy rows retain their pair/time with a null stored signature; new entries retain signatures. Revocations are never pruned.
- IndexedDB keeps its schema version and atomically replaces matching plaintext roots. Already signed-out legacy browsers have no Connect record identifying the root: sign in again to migrate. Previously exported backups cannot be removed remotely.

## Verification

All commands ran in the specified worktree. Final checks:

| Command | Result |
| --- | --- |
| `pnpm install` | Passed; workspace dependencies installed. `fake-indexeddb` added for transaction/abort regressions. |
| `cargo fmt --all --check` | Passed. |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed, including desktop targets. |
| `cargo test --workspace` | 245 passed, 0 failed, 4 ignored. Includes 17 Connect API tests and 13 server Connect integration tests. |
| `cargo test -p vc-proto --features ts` | Not run: neither vc-proto nor generated TS bindings changed. |
| `pnpm --filter web test` | 408 passed, 0 failed; 37 files passed. |
| `pnpm --filter web exec tsc --noEmit` | Passed. |
| `pnpm --filter web build` | Passed; Vite reports its existing large-chunk warning. |
| `cargo build -p vc-server -p gwar-connect` | Passed. |
| `WEB_URL=http://127.0.0.1:5193 SERVER_HTTP=127.0.0.1:8893 E2E_SCREENSHOTS=/private/tmp/gwar-integrity-e2e/screenshots pnpm --filter web e2e` | 57 passed, 0 failed, after service builds and a fresh web build. Uses throwaway local services and browser profiles. |

Ignored tests: `puppet_nicknames_follow_members_and_resolve_collisions`, `install_spawn_query_and_stop`, `query_against_running_server`, `supervisor_restarts_killed_server`. They require external TeamSpeak infrastructure and were not run. No ARM live TeamSpeak claim is made.

Focused checks also passed: `cargo test -p gwar-connect -p vc-server --test api --test connect` (30 passed, 0 failed). An earlier server test failed because its harness used nonexistent `server.info`; it was corrected to `ping`, then passed in focused and full workspace runs. An earlier type check found an unused import and a missing test-double `this` annotation; both were fixed. Final required checks have no failures.

Logs are under `/private/tmp/gwar-integrity-*`; e2e screenshots are under `/private/tmp/gwar-integrity-e2e/screenshots`.

## Limits

No requested implementation item is left out. The 90-day certificate decision is deferred as requested. The IndexedDB regression simulates transaction abort and the e2e reads real browser storage; no physical power-loss injection was performed. SFU removal dispatch/backpressure and routing removal are tested; no separate manual expired-certificate audio call or installed desktop UI session was performed.

A malicious Connect can withhold revocations or inventory and replay old encrypted vaults. Signatures do not prove service completeness or vault freshness. Revocation cannot erase copied vault keys, TeamSpeak credentials or root backups. These limits are documented in `docs/connect.md`; vault checkpoints, native key storage and broader transport/message hardening are outside this task.
