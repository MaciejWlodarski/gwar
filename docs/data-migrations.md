# Data migrations

Gwar updates itself: the web app at every deploy, the desktop app through its
updater, and servers whenever their admins choose. Whatever a version stored must
open in every later version, so an update never leaves a user without their
identity, keys, messages or settings. This document lists what Gwar keeps, the
version of each store, how each one moves forward, and how to change a format.

How clients and servers of different versions talk to each other is a separate
question: [compatibility.md](compatibility.md).

## Policy

- **Every format change ships a migration**, plus a test that opens data of every
  older supported version and ends at the current one with the data intact.
- **Never wipe identity, keys, MLS state, accounts or messages.** A store that
  cannot be read is left as it is and the failure is reported, never replaced by
  an empty one. Only pure caches may be dropped, and this document says which.
- **Refuse what is newer.** Data written by a newer version is neither read nor
  changed: an older version (a rollback, a downgraded app, an old tab) fails with
  an error that names the store and both versions, and leaves the data for the
  newer version to open again.
- **Crash-safe.** A migration runs in one transaction with the version change
  where the store has transactions (IndexedDB, SQLite), so an interrupted one
  commits nothing and runs again on the next open. Where it does not (files,
  localStorage), every step is idempotent and the version is written last.
- **Released migrations never change.** A database that ran one would differ from
  a new one. A mistake is fixed by a new migration.
- **Rollbacks.** A deploy that bumps a store version cannot be rolled back
  without stranding the users who already opened it with the new version: they
  get the "newer version" error until the deploy rolls forward again. Bump
  versions only in changes that are ready to stay.

## Supported versions

The baseline is every format written by a tagged release (v0.1.0 and later) under
the current `gwar` names, plus everything main has written since (the web app is
deployed from main). Concretely:

| Older data | Status |
| --- | --- |
| Message checks at IndexedDB version 2 (`gwar-message-auth-v3` before #105) | **Migrated** (2 → 3: adds the `order` store, existing checkpoints get order 0) |
| Server databases of v0.1.0: role permissions named as in roles version 1 | **Migrated** (with the roles v2 schema migrations: names mapped, Admin holds `administrator`, Member keeps joining, talking and writing) |
| Server databases of v0.1.0 that a server from v0.1.2 on already migrated, roles unconverted | **Repaired** (migration 31: only roles still holding a name of version 1 are converted, as above; roles edited since keep their version-2 names; a Member row still holding version-1 names also gains joining, talking and writing, which it missed with roles version 2 step 2; a Member row in version-2 names is never given anything, since an admin may have set it so; Member moves back to last place) |
| Server and Connect databases at any older `user_version` | **Migrated** (the ordered SQL migrations below) |
| TeamSpeak identity lists without a `version` (desktop, v0.1.0 on) | **Migrated** (read as version 1, written with it) |
| Web data under the old `vc` names (`vc` IndexedDB, `vc.*` localStorage: v0.1.0 and v0.1.1 in a browser) | **Unsupported.** The move ran only between #112 and #117; that data is ignored and left in place, and the app starts as on a new device (sign in to Gwar Connect again, or import an identity backup). |
| `vc.sqlite3` of a v0.1.0 or v0.1.1 server | **Unsupported automatically.** Stop the server, rename `vc.sqlite3` (and its `-wal`/`-shm`) to `gwar.sqlite3`, delete `vc.lock`, start the server: it then migrates as any older database. Without the rename the server starts with a new, empty database beside it. |
| `gwar-mls-v1-<device>` IndexedDB (before 2026-10-15) | **Unsupported**: ignored; the device rejoins its groups. The database is left in place. |
| `gwar-message-auth-v2` IndexedDB (before 2026-10-11) | **Unsupported**: ignored and left in place (a different database name, not read since). |
| Shapes only pre-v0.1.0 builds wrote: an identity without a nickname, a Gwar Connect sign-in without its vault key, bookmarks without `kind` or `updatedAt`, the single-identity TeamSpeak files (`teamspeak-identity*.json`), the single-identity vault form, invites without a creator | **Unsupported.** An identity the app cannot read fails to load with an error and is kept (never replaced by a new one); a sign-in without a vault key counts as signed out and is kept until the next sign-in replaces it; the old TeamSpeak files are not read; the rest is refused by the reader that meets it. |

## Inventory

### Web app and the desktop app's webview

The desktop app runs the same web app in its webview, so the same stores exist in
its WebKit data directory.

| Store | Holds | Version and mechanism |
| --- | --- | --- |
| IndexedDB `gwar`, store `kv` (`net/kv.ts`) | `identity`: the local identity, `{format: "gwar-identity", version: 1, jwk, nickname}` (the private key as a JWK; also the identity backup file format). `connect`: the Gwar Connect sign-in, `{version: 1, handle, nickname?, accountKey, deviceName, deviceJwk, certificate, token, vaultKey, archiveRecovery?, archiveRecoverySecret?}`. `moved-from-vc`: a timestamp left by the old rename move, unused. | Database version 1 (`KV_MIGRATIONS`). Each record has its own `version`, upgraded as it is read (`IDENTITY_UPGRADES`, `CONNECT_UPGRADES`, `upgradeRecord`). Every write and delete (save, sign-out, enrollment's removal of the old identity) reads the stored record in the same transaction and aborts if it is newer. **Never wiped**: an unreadable identity (including a key the browser rejects although it has Ed25519, which is not reported as a browser without Ed25519) fails to load and stays. |
| IndexedDB `gwar-message-auth-v3` (`net/message-storage.ts`) | `checkpoints` (by `server/author/message`, index `at`): `{revision, hash, created, conversation, at, proven?, deleted?, files?, order}`; `revocations` (by `account:device`): account-signed revocations; `feeds` (by Connect URL): `{cursor, checked, available?, read?}`; `order`: one counter. | Database version 3 (`MESSAGE_STORAGE_MIGRATIONS`; version 1 never held data, the name started at 2). Not a cache: losing it loses rollback protection, so it is migrated, never emptied. |
| IndexedDB `gwar-mls-v2-<device public key>` (`mls/storage.ts`) | The device's MLS agent: stores `openmls` (OpenMLS's own records), `groups` (`@device`, group records, `{server}/@server` owner pins, `@archive`), `epochs`, `outbox`; binary keys and values written by `crates/gwar-mls`. The runtime's jobs (`@runtime/…`, JSON) live in `outbox`. | Database version 1 (`MLS_MIGRATIONS`), which also versions the runtime's jobs. The agent's records have their own format, the `version` of `@device` (`STORE_FORMAT` in `crates/gwar-mls/src/agent/mod.rs`, currently 1): `STORE_UPGRADES` run as the agent loads and reach storage with its next write. OpenMLS's own records are keyed by its storage version, pinned by a test. **Never wiped**: an unreadable state fails to load and stays. |
| localStorage `gwar.settings` (zustand `persist`) | Preferences, bookmarks (with server and channel passwords), removed bookmarks, the last server, per-server flags. | The localStorage version (below). zustand's own `version` stays 1. |
| localStorage `gwar.server-pin:<endpoint>` | `{server_pk, first_seen}`: the server key first seen at an address. | The localStorage version. |
| localStorage `gwar.channels:<server key>` | `{known, deleted, incomplete}`: channel ids seen and seen deleted. | The localStorage version. A record it cannot read becomes "incomplete" (fails closed), never clean. |
| localStorage `gwar.connect.handle-pin:<Connect URL>:<handle>` | `{uid}`: the account first seen behind a handle. | The localStorage version. |
| localStorage `gwar.connect.service-key:<Connect URL>` | Gwar Connect's service key. | Cache; refetched. |
| localStorage `gwar.storage-version` | The version of all the keys above (`lib/local-storage.ts`); missing means 1. | — |
| Cache Storage `gwar-denoise` | The noise suppression module and model. | Cache: entries not in use are deleted. |

### Desktop app (Tauri)

| File (app data directory) | Holds | Version and mechanism |
| --- | --- | --- |
| `teamspeak-identities.json` | This device's TeamSpeak identities: `{version, default, identities: [{uid, name, identity}]}`. | `version` (`FORMAT` in `crates/gwar-client/src/ts_identities.rs`, currently 1; missing means 1), `UPGRADES` applied as it is read. A newer file is refused and left alone. Holds private keys: never wiped. |
| `teamspeak-identities.account.json` | The account's TeamSpeak identities from the vault, same format. | As above. Removed on sign-out (the vault keeps them). |
| `.window-state.json` (tauri-plugin-window-state) | Window size and position. | The plugin's own; a cache. |

The webview's data (above) is the rest of what the desktop app keeps.

### Server (`gwar-server`, data directory)

| File | Holds | Version and mechanism |
| --- | --- | --- |
| `gwar.sqlite3` | Everything the server knows: channels, roles, members, messages, files, invites, bans, MLS delivery state, owner chain, TeamSpeak links and imports. | `PRAGMA user_version` = the number of `MIGRATIONS` applied (`crates/gwar-server/src/store.rs`, currently 31). Each runs in one transaction with its data step (`migrate_data`) and the new version. A newer `user_version` refuses to start. |
| `gwar.sqlite3.before-import-*` | Copies taken before a TeamSpeak import. | The version they had then; they open like any older database. |
| `gwar.lock` | The data directory lock. | — |
| `server-key.ed25519` | The server's identity key, 32 raw bytes. | Fixed format; anything else fails to start. Never rewritten. |
| `files/` | Uploaded files, by id. | Opaque bytes, referenced from the database. |
| `acme/` | Let's Encrypt account and certificates (rustls-acme). | The library's cache. |
| `teamspeak/server-<version>/` | The pinned TeamSpeak server, with its `.installed` marker. | Reinstalled when the pinned version changes. |
| `teamspeak/state/` | The TeamSpeak server's own database and files. | Owned and migrated by TeamSpeak. |
| `teamspeak/query-password` | The ServerQuery password. | Plain text; kept. |
| `teamspeak/identities.json` | Puppet identities by Gwar uid (a JSON map). | Unversioned map: today's reader refuses anything but a map, so a future format must move to `{version, …}` (an older server then refuses it instead of misreading it). Losing it gives every member a new puppet. |
| `teamspeak/channels.json` | Gwar channel id → TeamSpeak channel id (a JSON map). | As above. |
| `teamspeak/server-password.json` | `{hash, password}`: the server password for puppets. | A cache of the plain text: unreadable means TeamSpeak users are kept out until the password is set again. |

### Gwar Connect (`gwar-connect`, data directory)

| File | Holds | Version and mechanism |
| --- | --- | --- |
| `connect.sqlite3` | Accounts, devices, revocations, sessions, vaults, challenges, the key archive, bug reports. | The first `SCHEMA`, then `PRAGMA user_version` = `MIGRATIONS` applied (`crates/gwar-connect/src/store.rs`, currently 5), each in one transaction. A newer `user_version` refuses to start. |
| `service-key.ed25519` | The service key that signs handle attestations. | Fixed format; never rewritten. |

The vault's contents are encrypted JSON that only clients read and write
(`apps/web/src/connect/vault.ts`): its format is the clients' to version, and
the server stores it as an opaque blob with a counter.

### Native client (`crates/gwar-client`)

The library itself keeps no files in the app: its `Identity::load_or_create`
(a base64url seed) and `teamspeak::load_identity` (tsproto JSON) are used only by
its examples. The TeamSpeak identity lists above are its format, written by the
desktop app.

## Mechanisms

### IndexedDB

`openDatabase(name, migrations)` (`apps/web/src/lib/stored-data.ts`) opens a
database at version `migrations.length`. In `onupgradeneeded` it runs
`migrations[oldVersion]` up to the last, all in the one version-change
transaction. A step that rewrites records returns the promise of `eachRecord`,
and the next step starts only once that one's rewrites have all succeeded, so
steps see each other's results in order. An exception, a rejected step or a
failed request aborts the transaction, the database keeps its old version and
data, and the reason (the step's error or the failed request's, not the abort it
caused) is what the open rejects with. A database of a newer version rejects with
`NewerDataError` without being changed. No step ever calls `createObjectStore`
outside its own migration.

At start the app also checks, without changing anything (`lib/stored-data-check.ts`):
the version of every database it knows (`indexedDB.databases()`), the `version`
of the identity and Connect records, and the agent format of every device's MLS
state (`@device`, against `MLS_STORE_FORMAT`, which a test keeps equal to the
newest fixture of `crates/gwar-mls`). Anything newer shows the "saved by a newer
version" screen instead of the app; each store still checks on its own open.
Failures that reach a user later (connecting, signing in, the identity settings)
are told apart in English and Polish: data of a newer version (update Gwar), an
identity this version cannot read (kept; update Gwar or import a backup), and
storage the browser refuses.

### Records inside IndexedDB

Records the app reads as objects carry `version`. `upgradeRecord(store, value,
upgrades)` runs the upgrades from the record's version on as it is read and
throws `NewerDataError` for a newer one; the upgraded record is written by the
next save. This keeps record changes independent of the database version, so a
record change does not lock every older tab out of the whole database. Writes
and deletes pass a guard to `kvSet`/`kvDelete`/`kvEnroll` that reads the stored
record in the write's own transaction, so an older tab can neither overwrite nor
delete a record a newer tab wrote meanwhile, and a newer record is reported as
such, never read as missing (signed out).

### MLS agent state

The agent's records are versioned together: `STORE_FORMAT` is the `version` of
the `@device` record. `Agent::load` reads it first; a newer format fails with
`AgentError::NewerFormat`, an older one goes through `STORE_UPGRADES` (functions
over the whole `Stores`). The upgraded state is not written by loading: the
agent keeps the stored state as its last durable one, so the upgrade is part of
the next `take_changes()` and lands in the same IndexedDB transaction as the
operation after it. Until then each load runs it again.

### localStorage

`gwar.storage-version` holds one version for every key the app keeps there
(missing: 1). `migrateLocalStorage` runs `LOCAL_STORAGE_MIGRATIONS` from it on
and writes the version after each step.

Each version keeps its entries under its own names: version 1 the plain ones
(`gwar.settings`), version n `gwar.vn.settings` (`localStorageKey`). A migration
copies the previous version's entries forward (`copyLocalStorageForward`,
changing them on the way) and never changes or removes them. localStorage has
no transactions, and across tabs not even a check followed by a write is atomic:
each tab's process writes its own copy and tells the others afterwards, so
neither a version check nor a lock (Web Locks only order the tabs, not when one
sees another's writes) stops a tab of the older version from writing just after
a newer tab upgraded. With separate names that late write lands in the older
version's entries, which the newer version never reads again, so it cannot
erase anything newer. Old entries may be deleted two versions later. Every key goes through the gate in
`lib/local-storage.ts` (`guardedLocalStorage` for the settings, `appStorage` for
the pins, channel records and service keys), which reads the version again
before each read and write, so a tab opened before a newer version's tab
upgraded localStorage stops writing at once. Other tabs also hear the change as
a `storage` event and replace the app with the error screen. A newer or
unreadable version, or a failed step, leaves everything untouched: nothing is
read or written, and the app shows the "saved by a newer version" screen.

### Desktop files

JSON files carry `version` (missing: 1). The loader refuses a newer version
before parsing the rest, applies the upgrades to the JSON, and the next save
writes the current version. A TeamSpeak identity list with an entry this
version cannot read is refused as a whole and left alone, rather than dropping
the entry and saving the rest (or a new identity) over it; skipping unreadable
entries is only for identities a user imports on purpose. Writes replace the file in one step (write a
temporary file, rename).

### Server and Connect databases

Ordered SQL in `MIGRATIONS`; `PRAGMA user_version` is how many ran. Each
migration, its data step and the new version commit in one transaction. A
`user_version` above `MIGRATIONS.len()` stops the process with an error naming
both versions; nothing is changed. A data step that a released server skipped is
repaired by a new migration that detects the old data itself (migration 31:
roles still named as in version 1), never by changing the step that was skipped.

## Adding a migration

Every kind has a test that fails when the version grows without a fixture:

- **IndexedDB database:** append a step to its `*_MIGRATIONS` array (never edit an
  existing one). Add a fixture for the new version to `IDB_STORES` in
  `apps/web/src/lib/data-migrations.test.ts`: `build` makes the database exactly
  as the code of that version did (including data), `check` opens it with the
  current code. The guard requires a fixture for every version, and the test
  opens each one at the current version, refuses a newer one and checks an
  interrupted upgrade.
- **Record in IndexedDB:** append an upgrade to `IDENTITY_UPGRADES` or
  `CONNECT_UPGRADES` (each takes a record of version n to n + 1; the version is
  set for you), change the record type and its writers to the new version, and
  add the old shape to `RECORD_FIXTURES`.
- **MLS agent records:** append to `STORE_UPGRADES`; `STORE_FORMAT` follows. Then
  write the new fixture with `GWAR_WRITE_MLS_FIXTURE=1 cargo test -p gwar-mls
  --lib stored_formats` and commit `crates/gwar-mls/tests/fixtures/agent-store-vN.json`.
  Older fixtures are never rewritten. Updating OpenMLS so its storage version
  changes fails `stored_formats_pin_the_openmls_storage_version`: that update
  needs a store upgrade too.
- **MLS runtime jobs:** a change to a job's shape is an `MLS_MIGRATIONS` step that
  rewrites the `@runtime/` records of `outbox`.
- **localStorage:** append to `LOCAL_STORAGE_MIGRATIONS` a step that calls
  `copyLocalStorageForward` (idempotent; never changes the previous version's
  entries) and add the old data to `LOCAL_FIXTURES`. Do not bump
  zustand's `version` for `gwar.settings`.
- **Desktop TeamSpeak identities:** append to `UPGRADES` in
  `crates/gwar-client/src/ts_identities.rs` and add the old file to its
  `FIXTURES`.
- **Server or Connect database:** append SQL to `MIGRATIONS`; a step that must
  rewrite rows goes in `migrate_data` (server), keyed by its migration. Append the
  new migration's digest to `MIGRATION_DIGESTS` (the test prints it); a changed
  digest of an existing one means a released migration was edited. Add a test that
  builds the database at the previous version with representative rows and
  checks them after `Store::init`, like `roles_of_version_1_become_roles_of_version_2`;
  `every_older_schema_opens_at_the_current_one_with_its_rows` covers the rest.
- **A new store:** give it a version from the start, add it to this inventory and
  to the guard test of its kind.
