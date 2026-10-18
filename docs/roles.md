# Roles and permissions, version 2

Status: design, for review. Replaces the flat role model described in
[architecture.md, "Identity and permissions"](architecture.md#identity-and-permissions).

## Why

Today a role is a name, a colour and a set of 15 server-wide permissions. That
is enough for a small server and too little for the communities Gwar wants to
bring over from TeamSpeak:

- **No order between roles.** Who may kick, ban or re-role whom follows from
  "nobody acts on someone holding permissions they lack". It works, but it
  cannot express Admin > Moderator > Veteran > Member, and people cannot see it.
- **Nothing per channel.** A TeamSpeak server lives on channel rules: a room
  only some roles can enter, a stage where only some speak, a channel without
  chat. Gwar can only hide a channel entirely (a private channel).
- **No voice moderation.** Nobody can mute someone for everyone.

TeamSpeak solves this with about 300 permissions and integer powers that even
its admins find hard. Gwar takes the model people already understand from
Discord and keeps it small: **ordered roles, a short list of permissions, and
per-channel overrides that inherit down the channel tree.**

## Concepts

### Roles

A role has a name, a colour, a **position**, its permissions and two display
flags:

- **Position.** Roles form one ordered list. The top of a member's roles is
  their **rank**. Admins reorder roles by dragging them.
- **Shown separately** (`hoist`): the member list groups online members under
  their highest such role instead of one "Online" group.
- **Mentionable**: `@Role` notifies everyone holding it (with the mention
  feature; until then the flag is stored only).

Built-in roles:

- **Member** is everyone's role: it is always last, cannot be deleted or given
  out, and its permissions are what a member without other roles has (today's
  member defaults).
- **Admin** starts first and holds `administrator`. It is an ordinary role in
  every other way: it can be renamed, recoloured and moved.

The server's **owner** (the signed owner chain) stands above every role.

### Permissions

Server permissions apply on the whole server. Channel permissions apply in
channels; roles carry them as defaults, and channels override them.

| Permission | Scope | Allows | Replaces |
| --- | --- | --- | --- |
| `administrator` | server | everything; overrides do not apply (rank still does) | the Admin group |
| `manage_server` | server | name, welcome message, password, uploads, server chat | `server_manage` |
| `manage_roles` | server | create, edit, reorder, delete and give roles below one's rank | `group_manage` |
| `manage_members` | server | change nicknames, remove members and prune | `member_nickname`, `member_remove` |
| `kick_members` | server | disconnect from the server | `client_kick` |
| `ban_members` | server | ban and unban | `client_ban` |
| `create_invites` | server | invites (for roles below one's rank) | `invite_create`, `token_create` |
| `view_channel` | channel | see the channel, its occupants and its chat | private channels' access (plaintext) |
| `connect` | channel | join its voice | TeamSpeak join power |
| `speak` | channel | be heard in its voice | TeamSpeak talk power |
| `bypass_password` | channel | join without the channel password | `channel_join_locked` |
| `send_messages` | channel | write in its chat | — |
| `attach_files` | channel | attach files | `file_upload` |
| `manage_messages` | channel | delete others' messages | `message_manage` |
| `move_members` | channel | move people into and out of it | `client_move` |
| `mute_members` | channel | mute or deafen someone for everyone in it | — (new) |
| `manage_channels` | channel | create subchannels, edit, move and delete it, edit its overrides | `channel_create`, `channel_edit`, `channel_delete` |

`manage_channels` at the server's top level (on the Member role or a role,
without an override) is what today's `channel_create` means for top-level
channels. Holders of `manage_channels` are the managers' candidates for the
owner's signed manager list, as `channel_edit` holders are today.

### Overrides

A channel may override any channel permission for a role or for one member,
each to **allow**, **deny** or (the default) **inherit**. A subchannel inherits
its parent's overrides unless it has its own (`sync with parent`, on for new
subchannels), so one rule on "Clan" covers every room under it.

### Resolving a permission

For member *m* in channel *c*:

1. The owner, or a member with `administrator`: allowed.
2. Start from the union of the permissions of *m*'s roles (Member included).
3. Take *c*'s effective overrides (its own, or its nearest ancestor's when
   synced), and apply them in order:
   1. the Member role's override;
   2. all of *m*'s other roles' overrides together: any **deny** removes, then
      any **allow** adds (an allow wins over a deny between two of *m*'s roles);
   3. *m*'s own member override.
4. Dependencies: without `view_channel` every other channel permission is
   off; without `connect`, `speak` is off.

This is Discord's order, so the result is what people expect from there; the
same function runs on the server (authoritative) and in the clients (to show
what one can do), checked against shared test vectors.

### Rank: who may act on whom

One rule replaces "nobody acts on someone holding permissions they lack":

- To kick, ban, move, mute, rename or remove a member, or to change their
  roles, one's rank must be **above** theirs (the owner's is above all; equal
  ranks cannot act on each other). `administrator` does not lift this.
- To create, edit, delete or give a role, it must be **below** one's rank, and
  one cannot give permissions one does not hold.
- To change a channel's overrides one needs `manage_channels` there, may only
  touch roles below one's rank and members ranked below oneself, and cannot
  allow a permission one does not have in that channel.
- Invites and tokens keep their rule: they work only while their maker could
  still give their roles.

## Private and encrypted channels

- A **plaintext private channel** becomes an ordinary channel with
  `view_channel` denied to Member and allowed to the admitted roles and
  members. "Private" stays as a label in the UI (a lock on the channel),
  and a hidden channel is still exactly as invisible as today: it, its
  occupants and its chat never reach those who cannot view it, and requests
  naming it answer "no such channel".
- **Rank decides who may give access.** Today a role (and the private
  channels it opens) is given out only by someone holding it or Admin. With
  ranks, anyone with `manage_roles` gives roles below their rank, so a role's
  channel access is trusted to everyone ranked above it. This is the main
  change in trust and the reason roles become ordered: the order is the
  admin's statement of who is trusted with what.
- **Encrypted private channels do not change.** Their admission is the member
  list signed by the owner or a manager and enforced by the MLS group
  (docs/e2e-build.md, step 6). Overrides cannot widen it: `view_channel` and
  `connect` there are the signed list, whatever the roles. Overrides can still
  narrow what the server controls in them anyway: `speak` (the SFU forwards
  nothing from a member who may not speak), `send_messages` (the server refuses
  to store) and `attach_files`.

## TeamSpeak

The bridge mirrors the result for official TeamSpeak clients (the core stays
the authority):

- A channel Member cannot view is not mirrored (as private channels today).
- `connect` and `speak` become the TeamSpeak channel's needed join and talk
  power. Each Gwar role a TeamSpeak user (`ts:<uid>`) holds is mirrored as a
  TeamSpeak server group `Gwar: <role>` carrying the join and talk power that
  role's allows need, and the bridge keeps their membership in sync.
- Puppets (Gwar users on TeamSpeak) keep powers above any channel's: the core
  already decided for them.
- Moderation from a TeamSpeak client is not supported: TeamSpeak users with
  Gwar roles moderate from the Gwar app.

This is also what the TeamSpeak importer ([teamspeak-import.md](teamspeak-import.md))
needs: join and talk power, channel admins and the group order map onto
`connect`, `speak`, member overrides with `manage_channels` and positions.

## Interface

- **Server settings → Roles.** A list ordered by position, dragged to
  reorder; one's own rank is marked and roles at or above it are read-only.
  Editing a role: name, colour, the two flags, and permissions grouped by
  topic (General, Members, Channels, Voice, Chat) with a description each and
  a search box.
- **Channel → Permissions.** The roles and members with overrides, plus "add
  role or member". Each permission is a three-way switch (deny / inherit /
  allow). Subchannels show "Synced with Clan" with a button to break the sync.
- **"Who can see this channel?"** under the list: the roles that end up with
  `view_channel`, so a rule's effect is visible before saving.
- **View as role.** An admin previews the channel tree and the chat as a role
  sees it. Later; not needed for the first release.
- Errors say why: "Moderator is above your rank", not "missing permission".

## Moving existing servers

One migration, on first start:

- Groups become roles: Admin first (with `administrator`), then the others in
  their current order of creation, Member last. Permissions map by the table
  above; `channel_create`, `channel_edit` and `channel_delete` all become
  `manage_channels`.
- Plaintext private channels become `view_channel` overrides.
- Encrypted private channels, the owner chain and the manager list stay as
  they are; managers are now drawn from `manage_channels` holders.
- Existing invites and tokens keep working under the new invite rule.

## Protocol

- `Group` gains `position`, `hoist`, `mentionable`; `Permission` gets the new
  names (the old ones are removed, not aliased: no backward compatibility
  before release).
- `Channel` gains `overrides` (`{ role | user, allow: [...], deny: [...] }`)
  and `synced`, sent to everyone who can view the channel.
- New requests: `group.reorder`, `channel.overrides` (set one role's or
  member's override), `member.voice` (mute or deafen someone for everyone).
- The welcome no longer has a private channel's `access`; `overrides` say the
  same.

## Building it

1. **Ranks**: positions, the rank rule, `administrator`, the new permission
   names, the migration, the roles page with dragging. Server, protocol,
   clients.
2. **Overrides**: storage, resolution with shared test vectors, plaintext
   private channels moved onto overrides, the channel permissions UI.
3. **Voice and TeamSpeak**: `speak` and `mute_members` in the SFU, the
   bridge's mirrored groups and powers.
4. **Then the importer**, onto this model.

Each step changes authorization, so each goes to an independent review before
it is merged.

## Open question

Should a role's channel access really be trusted down the order (above), or
keep today's stricter rule (only holders give a role) for roles that open a
hidden channel? The stricter rule needs no trust in the order, but it would
have to apply to every role (so a refusal never reveals a hidden channel),
which makes delegating roles to moderators awkward.
