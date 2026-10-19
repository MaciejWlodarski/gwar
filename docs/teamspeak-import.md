# Moving a TeamSpeak server to Gwar

Status: built (`vc-server import-teamspeak`), onto roles version 2 ([roles.md](roles.md)).
Later: a wizard in the desktop app, TeamSpeak 6.

A community with a TeamSpeak 3 server can move to a Gwar server without
rebuilding it by hand: `vc-server import-teamspeak` reads the TeamSpeak server
over ServerQuery and recreates its channels, roles, members, bans and settings.
Gwar runs the official TeamSpeak server next to it (see
[architecture.md, "TeamSpeak"](architecture.md#teamspeak)), so after the move
people keep joining with their TeamSpeak client at the same place and switch to
the Gwar app when they want; nobody has to install anything on moving day.

## What moves

| TeamSpeak | Gwar | Fidelity |
| --- | --- | --- |
| Channel tree: parent, order, name, topic, max clients | Channels, same tree and order | Exact |
| Default channel | `default_channel` | Exact |
| Channel password | The same password when the admin gives it; otherwise locked (see [Passwords](#passwords)) | Needs the admin |
| Channel description | Not imported (Gwar has a topic only) | Reported |
| Needed join power above the default group's | `connect` denied to Member, allowed to the roles with enough join power | Exact for those roles |
| Needed talk power above the default group's (moderated) | `speak` denied to Member, allowed to the roles with enough talk power | Exact for those roles |
| Channel groups (Channel Admin, Operator, …) | That member's override in that channel, allowing the channel permissions the group maps to, and `connect` or `speak` where the group's own join or talk power is enough | Per person |
| Temporary channels | Not imported | Reported |
| Semi-permanent channels | Permanent channels | Exact |
| Spacers (`[spacer]`, `[cspacer]`, …) | Not imported by default (`--spacers` keeps them as plain channels) | Reported |
| Server name, welcome message, max clients, password | Server settings (the password as channel passwords) | Exact; text cut to Gwar's limits, control characters dropped |
| Regular server groups (`type=1`) | Roles ranked by their powers, below Admin, permissions mapped (see [Roles](#roles-and-permissions)) | Lossy, reported per permission |
| The default server group (Guest) | Member's permissions, plus `connect`, `speak`, `send_messages` | As mapped |
| Template and query groups | Not imported | Reported |
| Group members | Members `ts:<uid>` with their roles | Exact |
| UID bans | Bans of `ts:<uid>` | Exact, expiry kept; expired ones skipped |
| IP bans that are a plain address | IP bans | Exact; a pattern is reported instead |
| Nickname and myTeamSpeak-id bans | Not imported | Reported |
| Icons, avatars, file browser | Not imported | — |
| Chat history | Nothing to import: TeamSpeak keeps none | — |
| Privilege keys | Not imported: create Gwar invites | — |

"Reported" means the dry run and the final report list every case by name, so
nothing disappears silently.

## Members and identities

A TeamSpeak user is known by their unique id, the base64 of the SHA-1 of their
identity's public key. The importer makes a member with uid `ts:<uid>` for every
client in a regular group (and, with `--all-clients`, for every client in the
database), with their last nickname, last-seen time and mapped roles. Clients in
no group but the default one are skipped by default: thousands of one-time
visitors would fill the member list.

When such a person joins Gwar's TeamSpeak server with their TeamSpeak client,
the bridge already looks the member up by `ts:<uid>` (`BridgeMsg::Join`), so
they arrive with their roles.

The Gwar app identifies people by an Ed25519 key, a different identity. A
TeamSpeak identity can be **linked** to a Gwar member, so the person keeps one
set of roles whichever app they use.

### Linked TeamSpeak identities

A member can have up to 64 TeamSpeak identities linked (`teamspeak_links`: the
TeamSpeak uid, the member, when); an identity is linked to at most one member.
The person links their own by a claim (below); the admin can link any with
`vc-server members link ts:<uid> <member-uid>` on a stopped server.

Linking `ts:<uid>` to member M is one transaction:

- If a member `ts:<uid>` exists (imported, or given roles in Gwar), its roles
  join M's (united, as in a merge), its per-channel member overrides move to M
  where M has none in that channel, and what the import last wrote for it
  follows M. The `ts:<uid>` member is then removed: its roles are M's from now
  on, and a later import counts it as removed in Gwar, so it does not come back.
- Messages, files, mentions and bans stay with `ts:<uid>`: they are what that
  TeamSpeak identity did, and its chat stays TeamSpeak chat (never an unsigned
  message of M). A banned identity cannot be claimed (`banned`); the admin can
  still link it, and the ban keeps refusing it.
- The link is public on this server: M's member entry lists its TeamSpeak
  uids, and a linked TeamSpeak user is shown with M's nickname.

A **linked TeamSpeak user** (someone joining Gwar's TeamSpeak server with that
identity) keeps the identity `ts:<uid>`, so their chat, direct messages and
mentions stay those of a TeamSpeak user, but acts as M: M's roles, M's
per-channel overrides and M's rank apply (in every channel move on TeamSpeak
too), and a ban of M or of `ts:<uid>` refuses them. Role changes, kicks and
bans of M reach that session too; lifting M's ban lifts the TeamSpeak bans the
bridge set for M's linked identities when it came (`ban_teamspeak`), also for
identities unlinked since. Their TeamSpeak visits keep M from
looking inactive (`last_seen`), and a later import gives a linked identity's
channel overrides to M.

**Unlinking** (`member.teamspeak.unlink`, by M or by someone with
`manage_members` ranked above M; or `vc-server members unlink ts:<uid>`) leaves
the roles with M. A connected session of that identity goes on as an unknown
TeamSpeak user, with Member's permissions. Removing M removes its links.
`members merge` moves the source's links to the target (refused past 64) and
refuses any `ts:` source: linking is the way to bring a TeamSpeak member to a
Gwar one.

### Claim

The person proves they hold the identity, with no admin work:

1. `teamspeak.challenge {}` answers `{nonce}`: 32 random bytes (base64url), one
   per session, valid for five minutes and usable once.
2. The app signs, with the identity's P-256 key (ECDSA with SHA-256, DER, the
   way a TeamSpeak client signs), the UTF-8 statement
   `gwar teamspeak claim v1\n{server_key}\n{uid}\n{nonce}`, where `server_key`
   is the server's key from the handshake and `uid` the claimant's.
3. `teamspeak.claim {key, signature}` sends the public key in TeamSpeak's own
   form (base64 of the libtomcrypt DER encoding, as `clientinitiv` carries it)
   and the signature (base64). The server derives the uid exactly as TeamSpeak
   does (base64 of the SHA-1 of the key re-encoded in that form), checks the
   signature and links the identity to the claimant. It answers
   `{teamspeak: "ts:<uid>"}`; `bad_request` for a malformed key or signature,
   `forbidden` for a wrong signature or no valid nonce, `conflict` when the
   identity is linked to another member, `banned` when it is banned,
   `quota_exceeded` past 64 linked identities. Only Gwar sessions claim, not
   TeamSpeak ones.

When the app offers it: `teamspeak.claimable {}` answers `{claimable}`, every
`ts:<uid>` member holding a role other than Member (at most 5000). These uids
are no secret: TeamSpeak shows them to everyone on its server, and Gwar's
member list shows those members. The desktop app asks for the list only when it
holds TeamSpeak identities and the person has not decided for this server yet.
It compares the list with its identities on the device and offers a claim only
for the ones in it. Nothing tells the server which identities the person holds
unless they accept, and declining is remembered for that server. A member's
profile lists their linked identities (unlink there); one's own profile can
also claim again later. The desktop's private keys never reach the web view:
the app signs only a statement of exactly this form, and only after the
person agrees in a native dialog naming the identity and showing the first
characters of the account and the server the statement binds (the app's own
dialog shows the same codes), so a compromised web view cannot give an
identity's roles away on its own or unnoticed. It could still ask about a
destination of its choice: a web view that is compromised controls the
person's Gwar session anyway, so the codes are the line of defence until the
desktop keeps the Gwar identity natively too. The list is asked for on
every visit, whatever matched or was answered before, so not even the requests
depend on the person's identities. The web app follows
later: it does not read TeamSpeak keys yet.

## Roles and permissions

TeamSpeak has around 300 permissions with integer powers; Gwar has a short list
([roles.md](roles.md)). A role gets a Gwar permission when its server group's
own assignments grant the TeamSpeak counterpart: a `b_` permission set to 1 and
not negated, or an `i_…_power` above 0 and at least the matching
`i_…_needed_…_power` of the default group, so the role can act on an ordinary
member. The report lists, per role, each permission and the assignment behind it.

| Gwar | TeamSpeak |
| --- | --- |
| `manage_server` | any of `b_virtualserver_modify_name`, `_welcomemessage`, `_password` |
| `manage_roles` | `b_virtualserver_servergroup_create`, or an `i_group_member_add_power` that reaches every imported group's needed power (adding to some groups only is reported, not mapped: `manage_roles` gives every role below) |
| `manage_members` | `b_client_modify_dbproperties` or `b_client_delete_dbproperties` |
| `kick_members` | `i_client_kick_from_server_power` |
| `ban_members` | `b_client_ban_create` |
| `manage_messages` | no counterpart (TeamSpeak keeps no chat): given with `b_client_ban_create` |
| `create_invites` | `b_virtualserver_token_add` |
| `bypass_password` | `b_channel_join_ignore_password` |
| `attach_files` | `i_ft_file_upload_power` |
| `move_members` | `i_client_move_power` |
| `mute_members` | `i_client_kick_from_channel_power` (the closest: keeping someone quiet in a channel) |
| `manage_channels` | `b_channel_create_permanent` or `b_channel_modify_name` |
| `connect`, `speak` | channel overrides from join and talk power (above) |

- `administrator` is never given: a group mapping to every permission is
  imported as such, and the admin ticks `administrator` if wanted.
- Roles go below Admin ranked by what they may do to others on TeamSpeak: the
  highest of their kick, ban, move and group powers, the sort id only breaking
  ties. TeamSpeak's sort id is a display order, and a list that shows
  moderators above admins must not let them act on the admins in Gwar. The
  report says when the two orders differ.
- Negated and skipped assignments count as not granting; client permissions
  are not read. A group whose assignments only restrict (negated or low
  powers) becomes a role without permissions, and the report says so.

## Passwords

A password cannot move by its hash. Measured on the official server 3.13.7 (CI,
draft PR #96): TeamSpeak clients send `base64(sha1(password))`, but the server
stores something else, a hash keyed with a secret of its own instance. The same
password gives a different `channel_password` on every fresh server, the same
one for the channel and the server password on one server, and it is neither
plain nor salted with the server's public unique id. ServerQuery also takes any
`channel_password` it is given as the plain password and hashes it again, so not
even another TeamSpeak server accepts the stored value. (Gwar's own storage,
Argon2 of `base64(sha1(password))`, would take the client's form directly; the
server-keyed one is what stands in the way.)

So the importer asks:

- It lists every channel with a password (and the server password, if set),
  and the admin types the password to keep, from the terminal or a file
  (`--passwords passwords.txt`, see [Running it](#running-it)). A typed
  password works exactly as before, for Gwar users and for TeamSpeak clients
  alike.
- The bridge sets a TeamSpeak channel's password only from the plain text (the
  reason above). With `--for-bridge` (the server runs with `--teamspeak`) the
  importer leaves each typed password in `ts_import_passwords`, on every run; the bridge
  sends it on attach and the row goes once TeamSpeak has taken it, or when the
  channel's password changes in Gwar. Without `--for-bridge` none is written,
  and a server started without TeamSpeak support clears the table. The
  database overwrites what it deletes (`secure_delete`) and empties its
  write-ahead log after, so no copy stays on disk; backups made while a
  password waits contain it, and are readable by their owner only.
- A channel whose password is not given stays protected: it is imported with a
  random password and listed in the report, so the admin sets a new one later.
  An import never opens a protected channel to everyone.
- With `--drop-passwords` they are imported without a password instead, also
  listed in the report.

## Running it

```sh
ssh -L 10011:localhost:10011 ts.example.com   # raw ServerQuery is unencrypted
VC_TS_QUERY_PASSWORD=… vc-server import-teamspeak --from 127.0.0.1:10011 [--user serveradmin] \
  [--sid 1] [--dry-run] [--all-clients] [--spacers] [--replace-channels [--force]] \
  [--passwords passwords.txt | --drop-passwords] [--for-bridge]
```

- The ServerQuery password comes from `VC_TS_QUERY_PASSWORD` or, with
  `--password-stdin`, the first line of stdin; it is used for this run only.
  Raw ServerQuery sends it unencrypted, so `--from` must be this machine (an
  SSH tunnel, as above) unless `--insecure-query`. The source server is only
  read (the importing host must be allowed by its `query_ip_allowlist.txt`, or
  its flood limits apply).
- The import is all or nothing: it runs on a copy of the database
  (`vc.sqlite3.importing`), which replaces the database only once everything
  went in. `--dry-run` does the same and throws the copy away, so it shows
  exactly what would change (also while the server runs).
- With the server stopped, the database as it was stays next to it
  (`vc.sqlite3.before-import-<time>`); the new one keeps its owner and mode. A
  server that never ran gets a new database, with the imported channels
  instead of the seeded ones.
- **While the server runs** (it holds `data/vc.lock`), the import goes through
  the server instead: the command first makes the same dry run (on a copy, so
  nothing is applied if that fails), keeps the database as it was
  (`vc.sqlite3.before-import-<time>`), then hands the plan to the running
  server over its local control socket (`data/admin.sock`, a Unix socket only
  the server's own system user can use). The server applies it in one
  transaction (all or nothing, like the stopped import), reloads its channels,
  roles, settings and password, and then everyone connects again, as after a
  restart that is not one: Gwar apps reconnect by themselves within seconds
  and see the server as it is now, and the TeamSpeak bridge reconnects to
  take the new channels and passwords (TeamSpeak users stay on their server).
  Requests wait while the server applies the import; hashing the passwords of
  protected channels takes most of that. `--replace-channels` needs the server
  stopped, and typed passwords wait for the bridge only on a server running
  with TeamSpeak.
- `--passwords` takes one `name = password` per line (`#` comments, quotes
  optional): a channel's path (`"Clan/Room"`; a `/` or `\` in a name is
  written `\/`, `\\`), a name no other imported channel has, or `@server`
  for the server's. A name several channels share, or one that matches no
  password channel, stops the import: a password never lands where it was not
  meant.
- Running it again later updates what it imported before instead of adding
  copies (`ts_imports`, by the source server's unique id), but only what nobody
  changed in Gwar since: each record keeps what the import last wrote. A
  channel, role, Member's permissions, the role order, the default channel or
  the server's settings edited in Gwar stay as they are (what Gwar changes by
  itself, such as overrides that go with a deleted role or follow a merged
  or linked member, is no edit); anything deleted in Gwar is not made again;
  a role taken from a member in Gwar is not given back, and a ban lifted in
  Gwar stays lifted. The report lists each. A TeamSpeak member linked to a
  Gwar member counts as removed: their roles from then on are managed in Gwar. A member's roles follow TeamSpeak
  otherwise: one added there is given, one removed there goes. New TeamSpeak
  items are added. So a community can import, try Gwar, and import again just
  before switching.
- Imported channels go after the existing ones; a name taken among siblings
  gets ` (2)`. `--replace-channels` removes the server's own top-level
  channels first (the seeded Lobby, General, AFK; the default channel stays
  unless TeamSpeak's replaces it), but none holding messages, subchannels or
  encrypted history unless `--force`.
- Imported channels have their own overrides (`synced` off): TeamSpeak has no
  inheritance to follow.

## Scope

Built: the CLI above, TeamSpeak 3 servers (3.13) over the raw ServerQuery
port, which `vc-server` already speaks for the bridge. Later:

- a wizard in the desktop app's server settings (the app can open TCP; the web
  app cannot);
- ServerQuery over SSH and TeamSpeak 6 servers (no raw ServerQuery port, and a
  different uid hash, so `ts:<uid>` would not match);
- channel descriptions, icons.

## Tests

- Unit (`src/ts_import/tests.rs`): permission mapping, ranks by power, the
  plan (tree, order, join and talk power, channel groups, passwords and how
  they are matched, members, bans, notes), applying twice without change, a
  second import after edits in Gwar, roles following TeamSpeak,
  `--replace-channels` and `--force`, the password queue, the all-or-nothing
  copy and the dry run.
- Core (`core::import_password_tests`): a queued password is sent on attach and
  kept until the bridge confirms it; without TeamSpeak none is kept.
- Integration (`tests/teamspeak_import.rs`, CI with the official server): the
  TeamSpeak server Gwar runs, given a group, powers and a password through
  ServerQuery, is read, planned and imported into a fresh database.
