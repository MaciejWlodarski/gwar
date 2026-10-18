# Moving a TeamSpeak server to Gwar

Status: built (`vc-server import-teamspeak`), onto roles version 2 ([roles.md](roles.md)).
Later: the claim flow for roles, a wizard in the desktop app, TeamSpeak 6.

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
| Channel groups (Channel Admin, Operator, …) | That member's override in that channel, allowing the channel permissions the group maps to | Per person |
| Temporary channels | Not imported | Reported |
| Semi-permanent channels | Permanent channels | Exact |
| Spacers (`[spacer]`, `[cspacer]`, …) | Not imported by default (`--spacers` keeps them as plain channels) | Reported |
| Server name, welcome message, max clients, password | Server settings (the password as channel passwords) | Exact |
| Regular server groups (`type=1`) | Roles in TeamSpeak's sort order, below Admin, permissions mapped (see [Roles](#roles-and-permissions)) | Lossy, reported per permission |
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

The Gwar app identifies people by an Ed25519 key, a different identity. Two ways
to bring a TeamSpeak member's roles to their Gwar identity:

1. **Claim (planned, second step).** The desktop app holds the person's
   TeamSpeak identities (docs/connect.md, "TeamSpeak identities"). It proves
   ownership by signing a server challenge with the identity's P-256 key and
   sending the public key; the server checks that the SHA-1 of the key is the
   member's uid and the signature, then merges `ts:<uid>` into the person's
   member (`members merge` semantics: roles united, nothing else lost). No
   admin work and nothing to trust but the key.
2. **By hand, today:** `vc-server members merge ts:<uid> <uid>` on a stopped
   server, or the same from the members tab later.

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
| `manage_roles` | `b_virtualserver_servergroup_create`, or `i_group_member_add_power` |
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
- Roles go below Admin in TeamSpeak's sort order (lowest sort id on top), so
  rank follows TeamSpeak's display order; reorder them after if TeamSpeak's
  powers said otherwise.
- Negated and skipped assignments count as not granting; client permissions
  are not read.

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
  (`--passwords passwords.toml`: `"Channel name" = "password"`, with
  `server = "…"`). A typed password works exactly as before, for Gwar users and
  for TeamSpeak clients alike. The bridge sets a TeamSpeak channel's password
  only from the plain text (the reason above), so the importer leaves each typed
  password in `ts_import_passwords` for the bridge's first sync, which applies
  it and deletes the row; without the bridge (no TeamSpeak support) none is
  written.
- A channel whose password is not given stays protected: it is imported with a
  random password and listed in the report, so the admin sets a new one later.
  An import never opens a protected channel to everyone.
- With `--drop-passwords` they are imported without a password instead, also
  listed in the report.

## Running it

```sh
VC_TS_QUERY_PASSWORD=… vc-server import-teamspeak --from ts.example.com:10011 [--user serveradmin] \
  [--sid 1] [--dry-run] [--all-clients] [--spacers] [--replace-channels] \
  [--passwords passwords.txt | --drop-passwords]
```

- The ServerQuery password comes from `VC_TS_QUERY_PASSWORD` or, with
  `--password-stdin`, the first line of stdin; it is used for this run only.
  The source server is only read (the importing host must be allowed by its
  `query_ip_allowlist.txt`, or its flood limits apply).
- `--dry-run` reads the TeamSpeak server and prints the full plan and report;
  it does not touch the Gwar database.
- Otherwise the Gwar server must be stopped (`data/vc.lock`). The import first
  writes a copy of the database next to it (`vc.sqlite3.before-import-<time>`),
  then applies the plan.
- `--passwords` takes one `name = password` per line (`#` comments, quotes
  optional): a channel's path (`"Clan/Room"`), its name, or `server`.
- Running it again later updates what it imported before instead of adding
  copies (`ts_imports`, by the source server's unique id). Items deleted on
  the Gwar side since are created again; new TeamSpeak items are added. So a
  community can import, try Gwar, and import again just before switching.
- Imported channels go after the existing ones; a name taken among siblings
  gets ` (2)`. `--replace-channels` removes the server's own top-level
  channels first (the seeded Lobby, General, AFK; the default channel stays
  unless TeamSpeak's replaces it).
- Imported channels have their own overrides (`synced` off): TeamSpeak has no
  inheritance to follow.

## Scope

Built: the CLI above, TeamSpeak 3 servers (3.13) over the raw ServerQuery
port, which `vc-server` already speaks for the bridge. Later:

- the claim flow for roles (above);
- a wizard in the desktop app's server settings (the app can open TCP; the web
  app cannot);
- ServerQuery over SSH and TeamSpeak 6 servers (no raw ServerQuery port, and a
  different uid hash, so `ts:<uid>` would not match);
- channel descriptions, icons.

## Tests

- Unit (`src/ts_import/tests.rs`): permission mapping, the plan (tree, order,
  join and talk power, channel groups, passwords, members, bans, notes),
  applying twice, `--replace-channels`, the passwords file.
- Integration (`tests/teamspeak_import.rs`, CI with the official server): the
  TeamSpeak server Gwar runs, given a group, powers and a password through
  ServerQuery, is read, planned and imported into a fresh database.
