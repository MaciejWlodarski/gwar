# Hostile input hardening

Implemented in `/Users/zireael/dev/projects/gwar-hostile-input`, branch
`hostile-input`, on 2026-10-11. Eight commits, one per item. Client resource
bounds share the file-transport commit because URL checks and bounded fetching
use the same path. Every commit has the requested GPT-6.1-Sol co-author footer.
No push, deployment, remote-machine changes or stash.

## Items and regression coverage

1. **File URL confinement.** Web accepts only `GET /files/{id}/{name}` and
   `PUT /api/files/{id}?token=...`, resolved against the locally selected
   connection origin. Absolute URLs, credentials, traversal, nested escapes,
   other routes/schemes and all redirects fail. Rust stores the connected
   origin through `files_set_server`, clears it on disconnect/reconnect, checks
   paths/methods itself and builds every URL. DNS answers are checked and pinned;
   public names cannot resolve to private/loopback/link-local targets. LAN
   literals, localhost, local hostnames and `.local` origins may reach their own
   pinned addresses. Reqwest has no proxy or redirects, a ten-second connect
   deadline, a thirty-second request deadline, a thirty-MiB ceiling and four
   concurrent file slots. Saving fetches before writing to the chosen file.
   Tests: web `file URL confinement`; native `paths_and_methods_are_confined`,
   `public_hosts_cannot_resolve_to_lan_targets`, `size_caps_and_names`,
   `redirects_overruns_timeouts_and_disconnects_fail`,
   `native_requests_cancel_without_waiting_for_timeout`.

2. **Capability URLs and caching.** Architecture docs state that anyone who
   knows a download URL can fetch it, without a session. Channel/server removal
   does not revoke known URLs; deleting the stored file removes server access.
   Upload tokens expire after ten minutes and work once. Download capabilities
   do not expire, so all file responses use `Cache-Control: no-store`, including
   media. This satisfies the round-2 correction without implying token expiry.
   `nosniff`, sandbox CSP and Content-Disposition remain. Test:
   `ranges_stream_only_the_slice_and_keep_security_headers`; e2e also checks
   image type and non-media attachment disposition.

3. **Resource bounds.** `fetchFileBytes(path, {origin, size, uploadLimit, signal})`
   in `platform/download.ts` streams display/download bodies within the declared
   size, server upload limit and local thirty-MiB ceiling. It rejects oversized
   Content-Length and streamed overruns, times out after thirty seconds and
   cancels on unmount. Images/audio/video receive bounded blobs; saving a web
   file uses an octet-stream download blob. Native requests also cancel on
   disconnect. Uploads use fetch with redirect rejection instead of XHR;
   progress now indicates completion rather than byte-level upload progress.
   Server GET/range bodies seek and stream only the slice in at most sixty-four
   KiB chunks. Pre-auth limits run before password Argon2: sixteen live
   connections per IP, 1024 globally, twenty connection/hello attempts per IP
   with one token per second, an 8192-entry IP table, and four concurrent Argon2
   checks with no waiting queue. Frames/messages are capped at ninety-six KiB
   before parsing; ping traffic cannot extend the hello deadline.
   Tests: web `bounded file fetching`, including real redirect and stalled-body
   tests; native file tests above; server
   `ranges_stream_only_the_slice_and_keep_security_headers` (one byte of a
   sparse eight-GiB file),
   `connections_are_limited_per_ip_before_upgrade_and_release_on_drop`,
   `hello_rate_and_ip_table_are_bounded`, `password_work_has_no_unbounded_queue`,
   `unauthenticated_connections_are_limited_before_hello_or_password`,
   `oversized_hello_is_closed_before_json_parsing`.

4. **Runtime schemas.** Chose small handwritten validators, with no new
   dependency. Mapped schemas are checked against generated protocol types and
   cover every event and response operation. Exact object shapes, string/array
   limits and safe integer ranges reject malformed data before delivery. UID
   values that collide with Object.prototype keys are rejected too. Frames are
   capped at two MiB before JSON parsing. Invalid frames warn and drop;
   operation-specific checks prevent a valid payload for another response type
   from reaching request consumers. TeamSpeak native welcomes/replies receive
   the same validation. The limited `connection.ts` edit is required for this
   per-operation check. Tests: `bounded server frames`: valid events/replies,
   wrong types, huge strings/arrays, thousand-level nesting, unsafe integers,
   prototype-pollution keys, voice-slot bounds and deterministic fuzz mutations.

5. **CSP.** Theme bootstrap is `/theme.js`. Vite inserts the strict CSP meta
   policy only for production builds; dev omits it for HMR and local smoke tests.
   Production web permits local scripts and `wasm-unsafe-eval`, HTTPS/WSS
   connections and self/blob/HTTPS media; objects, base URLs and forms are
   disabled. `build:desktop` adds native IPC/LAN transport permissions; the
   native CSP retains its own policy. Deploy docs contain the required HTTP
   headers and nginx inheritance instructions. Tests: `csp.test.ts`; actual
   production Chromium checks in `e2e/csp.mjs` verify static theme loading,
   blocked inline script and hostile base URI, real hash-wasm Argon2 execution,
   and blocked HTTP connections. Vite dev works in the full e2e suite.

6. **Connect transport.** API and service-key fetching share `connectJson` and
   `connectUrl`: HTTPS only, dev loopback HTTP exception, no URL credentials,
   no redirects, omitted cookies, no-store, ten-second deadline covering the
   body and four-MiB JSON cap. Tests: `Connect transport`, the real stalled JSON
   body test, existing `ConnectApi` and `service key trust anchor` suites.

7. **Native secrets and IPC.** Routine `ts_identity_list` returns source/default
   and UID/name metadata. Export and official-client discovery require a
   Rust-owned Tauri confirmation dialog before reading/returning private keys.
   Vault seeding uses this confirmed export. Public UID references allow
   metadata edits without exporting keys. Native TeamSpeak connection already
   loaded the identity directly in Rust and continues to receive only its UID
   from JS. Removed unused debug snapshot IPC and file-driven webview evaluation;
   narrowed `core:default` to event listen/unlisten permissions. Architecture
   docs list every retained command and its risk. Tests:
   `identity_list_serializes_public_metadata_only`,
   `public_edits_preserve_native_keys`,
   `connecting_picks_the_asked_identity_then_the_default`, plus the TeamSpeak
   vault/list and connection suites.

8. **Image dimensions.** Clamp declared dimensions to 1–16384 pixels before
   computing a finite 400×300 layout box. Non-finite and negative values cannot
   produce invalid layout dimensions. Test: `clamps hostile dimensions before
   layout`, plus existing fit-box tests and image/lightbox e2e checks.

## Verification

Final successful runs, after fixes:

- `cargo fmt --all --check`: exit 0.
- `cargo clippy --workspace --all-targets -- -D warnings`: exit 0.
- `cargo test --workspace`: 245 passed, 0 failed, 4 ignored.
- `pnpm --filter web test`: 415 passed, 0 failed, 40 test files.
- `pnpm --filter web exec tsc --noEmit`: exit 0.
- `pnpm --filter web build`: exit 0. Existing Vite bundle-size warning remains.
- `cargo check -p vc-desktop`: exit 0.
- `pnpm --filter web build:desktop`: exit 0.
- `cargo check -p vc-desktop --features custom-protocol`: exit 0.
- `cargo build -p vc-server -p gwar-connect`, followed by a fresh
  `pnpm --filter web build`: both exit 0 before e2e.
- `WEB_URL=http://127.0.0.1:65489 SERVER_HTTP=127.0.0.1:65490 pnpm --filter web e2e`:
  57 passed, 0 failed. Free TCP/UDP ports were selected for this run. Screenshot
  output: `/private/tmp/hostile-input-e2e-final`.
- `pnpm --filter web exec node e2e/csp.mjs`: 5 passed, 0 failed.
- `git diff --check`: exit 0.

Installed dependencies were present and no dependencies changed, so `pnpm
install` was unnecessary. The first e2e run was 55/57: bounded image fetching
exposed missing CORS permission for the random web origin. The test harness now
passes `--web-origin` for that origin; the final run also saves and reads back
an actual downloaded file.

The four ignored tests require TeamSpeak infrastructure:
`puppet_nicknames_follow_members_and_resolve_collisions`,
`install_spawn_query_and_stop`, `query_against_running_server`,
`supervisor_restarts_killed_server`.

## Hosting snippet

Use the following in `snippets/gwar-security-headers.conf`. Include it at the
HTTPS server level and again in each location declaring its own `add_header`,
including static `/assets/` and `/`, as shown in `deploy.md`.

```nginx
add_header Content-Security-Policy "frame-ancestors 'none'" always;
add_header X-Content-Type-Options "nosniff" always;
add_header Referrer-Policy "no-referrer" always;
add_header Strict-Transport-Security "max-age=31536000" always;
```

## Integration and limits

- The handshake/signing branches must extend the schemas for their new wire
  fields and enums. Unknown fields deliberately fail closed. Attachment hash
  verification and explicit loading of invalid/unsigned signed attachments
  belong in the signing integration; they can build on `fetchFileBytes`.
- No native confirmation dialog was manually clicked, and no packaged desktop
  build was run. Native tests and both desktop Cargo checks passed on this host;
  the real CSP browser check used Chromium, not the Tauri webview.
- Confirmed export and encrypted account sync still pass TeamSpeak private text
  to JS. Gwar signing-key migration into native protected storage is outside
  these items. Layout/transfer bounds do not bound decoded media memory.
- Capability URLs remain bearer URLs without download expiry, as documented;
  no-store does not revoke a copy someone already saved.
