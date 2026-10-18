# Moving a TeamSpeak server to Gwar

Status: design. Nothing here is built yet.

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
| Channel password | The same password when the admin types it; otherwise locked (see [Passwords](#passwords)) | Needs the admin |
| Channel description | Not imported (Gwar has a topic only) | Reported |
| Join power above the default group's | Private channel open to the roles that have the power | Changed: hidden instead of visible but locked; reported |
| Needed talk power (moderated channels) | Not imported | Reported |
| Temporary channels | Not imported | Skipped |
| Semi-permanent channels | Permanent channels | Exact |
| Spacers (`[spacer]`, `[cspacer]`, …) | Not imported by default (`--spacers` keeps them as plain channels) | Reported |
| Server name, welcome message, max clients | Server settings | Exact |
| Server password | The same password when the admin types it | As channel passwords |
| Regular server groups (`type=1`) | Roles, permissions mapped (see [Roles](#roles-and-permissions)) | Lossy, previewed |
| Template and query groups | Not imported | Skipped |
| Group members | Members `ts:<uid>` with their roles | Exact |
| Channel groups (Channel Admin, Operator) | Not imported | Reported (count per channel) |
| UID bans | Bans of `ts:<uid>` | Exact, expiry kept |
| IP bans that are a plain address | IP bans | Exact; a pattern is reported instead |
| Nickname and myTeamSpeak-id bans | Not imported | Reported |
| Icons, avatars, file browser | Not imported | Reported |
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

TeamSpeak has around 300 permissions with integer powers; Gwar has 15 yes/no
permissions. A role gets a Gwar permission when its server group's own
assignments grant the TeamSpeak counterpart: a `b_` permission set to 1 and not
negated, or an `i_…_power` at least the matching `i_…_needed_…_power` of the
server's default group, so the role can act on an ordinary member.

| Gwar | TeamSpeak |
| --- | --- |
| `server_manage` | any of `b_virtualserver_modify_name`, `_welcomemessage`, `_password` |
| `channel_create` | `b_channel_create_permanent` or `b_channel_create_semi_permanent` |
| `channel_edit` | `b_channel_modify_name` |
| `channel_delete` | `b_channel_delete_permanent` |
| `channel_join_locked` | `b_channel_join_ignore_password` |
| `client_move` | `i_client_move_power` |
| `client_kick` | `i_client_kick_from_server_power` |
| `client_ban` | `b_client_ban_create` and `i_client_ban_power` |
| `group_manage` | `b_virtualserver_servergroup_create` or `i_group_member_add_power` |
| `token_create`, `invite_create` | `b_virtualserver_token_add` |
| `message_manage` | no counterpart (TeamSpeak keeps no chat): given with `client_ban` |
| `file_upload` | `i_ft_file_upload_power` |
| `member_remove` | `b_client_delete_dbproperties` |
| `member_nickname` | `b_client_modify_dbproperties` |

- The group named in `virtualserver_default_server_group` (Guest) becomes
  Gwar's built-in Member role: its mapped permissions replace Member's, after
  the dry run shows the difference.
- A group whose mapping grants every Gwar permission is imported as a role, not
  merged into Gwar's built-in Admin; the server's owner and Admin stay as they
  are.
- Group colours do not exist in TeamSpeak (it has icons); imported roles have
  none.
- Negated (`permnegated`) and skipped (`permskip`) assignments count as not
  granting; client- and channel-specific permissions are not read.

The mapping is a starting point: the dry run prints, per group, the Gwar
permissions it would get and the TeamSpeak assignments behind each, and roles
can be edited after the import as usual.

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
vc-server import-teamspeak --from ts.example.com:10011 --user serveradmin \
  [--port 9987 | --sid 1] [--dry-run] [--all-clients] [--spacers] \
  [--passwords passwords.toml | --drop-passwords] [--report report.json]
```

- The ServerQuery password is read from the terminal (or `--password-stdin`),
  used for this run only and never stored. The source server is only read.
- The Gwar server must be stopped (`data/vc.lock`, as `members merge`), even
  for `--dry-run`, which reads the TeamSpeak server and prints the full plan
  without writing.
- The import runs in one transaction: it applies fully or not at all.
- Running it again later updates what it imported before instead of adding
  copies: `ts_imports(source, kind, ts_id, gwar_id)` remembers each imported
  item by the source server's unique id. Items deleted on the Gwar side since
  stay deleted; new TeamSpeak items are added. So a community can import,
  try Gwar, and import again just before switching over.
- Imported channels go after the existing ones; an existing empty server (the
  default Lobby, General, AFK) can be replaced with `--replace-channels`.
- Requests are paced to stay under ServerQuery's flood limits; the client
  database is read in pages. The source must allow the importing host in its
  `query_ip_whitelist.txt` or the default flood limits apply.

## Scope

First version: the CLI above, TeamSpeak 3 servers (3.13) over the raw
ServerQuery port, which `vc-server` already speaks for the bridge. Later:

- the claim flow for roles (above);
- a wizard in the desktop app's server settings (the app can open TCP; the web
  app cannot);
- ServerQuery over SSH and TeamSpeak 6 servers (no raw ServerQuery port, and a
  different uid hash, so `ts:<uid>` would not match);
- channel descriptions, channel groups, icons.

## Tests

- Unit: permission mapping, channel tree reconstruction (`channel_order` is the
  id of the sibling above), ban conversion, password import, re-run updates.
- Integration (CI, the official TeamSpeak server it already runs; its free
  license allows one instance per host, so the steps run one after another):
  start a plain TeamSpeak server as the source and build it up through
  ServerQuery (tree, passwords, groups, members, bans); import into a stopped
  Gwar server; stop the source; start Gwar with its bridge and check the result,
  and that a TeamSpeak client joins a password channel with the original
  password and arrives with its role.
