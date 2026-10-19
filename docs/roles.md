# Roles and permissions, version 2

Status: steps 1 (ranks), 2 (overrides), 3 (voice; TeamSpeak talk power still open) and 4 (the TeamSpeak importer) built. Replaces the flat role model.

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
| `connect` | channel | join its voice | TeamSpeak join power |
| `speak` | channel | be heard in its voice | TeamSpeak talk power |
| `bypass_password` | channel | join without the channel password | `channel_join_locked` |
| `send_messages` | channel | write in its chat | — |
| `attach_files` | channel | attach files | `file_upload` |
| `manage_messages` | channel | delete others' messages | `message_manage` |
| `move_members` | channel | move people into and out of it | `client_move` |
| `mute_members` | channel | mute or deafen someone for everyone in it | — (new) |
| `manage_channels` | channel | create subchannels, edit, move and delete it, edit its overrides | `channel_create`, `channel_edit`, `channel_delete` |

Step 1 has the twelve that replace today's permissions; `connect`, `speak`,
`send_messages` (step 2) and `mute_members` (step 3) come with overrides and
voice moderation.

Who **sees** a channel is not a permission: a channel is public, or private
and admits the roles and members on its list (below, "Private and encrypted
channels"). Seeing it is the condition for every channel permission there.

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
4. Dependencies: in a channel one is not admitted to (a private channel not
   listing them) every channel permission is off; without `connect`, `speak`
   is off.

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

- **Private channels stay the way to hide a channel** (decided while building
  step 2, 2026-10-19). The design first turned a plaintext private channel
  into `view_channel` overrides. Private channels already do exactly that
  job (a list of roles and members, server-enforced, reviewed for not
  revealing hidden channels by name, occupants, errors or children), so a
  second way to hide would only add places for the two to disagree. Overrides
  apply inside a channel one sees, private or not: "visible, but only Crew may
  join" is `connect` denied to Member and allowed to Crew.
- **Rank decides who may give access.** Today a role (and the private
  channels it opens) is given out only by someone holding it or Admin. With
  ranks, anyone with `manage_roles` gives roles below their rank, so a role's
  channel access is trusted to everyone ranked above it. This is the main
  change in trust and the reason roles become ordered: the order is the
  admin's statement of who is trusted with what.
- **Encrypted private channels do not change.** Their admission is the member
  list signed by the owner or a manager and enforced by the MLS group
  (docs/e2e-build.md, step 6). Overrides cannot widen it: seeing and joining
  there are the signed list, whatever the roles. Overrides can still
  narrow what the server controls in them anyway: `speak` (the SFU forwards
  nothing from a member who may not speak), `send_messages` (the server refuses
  to store) and `attach_files`.

## TeamSpeak

The core stays the authority for official TeamSpeak clients too:

- Private channels are not mirrored (as before).
- `connect`: a TeamSpeak user who arrives in, or moves into, a channel they
  may not join stays where they were in Gwar (the default channel on
  arrival), and the bridge moves them back there on TeamSpeak. One who loses
  `connect` where they are goes to the default channel, on TeamSpeak too. Should
  TeamSpeak refuse that move too, the bridge kicks them from the channel
  (to TeamSpeak's default one) rather than retry. The default channel itself
  is not checked: a server that denies Member `connect` there leaves arriving
  TeamSpeak users in it, unheard.
- `speak` and moderators' mutes: Gwar users never hear a TeamSpeak user who
  may not speak there or was muted for everyone: the SFU drops their audio
  like anyone's, and they do not show as talking.
- Deafening for everyone is refused for TeamSpeak users: they hear Gwar
  through the puppets in their TeamSpeak channel, which a deafen does not
  reach.
- Not yet: TeamSpeak users in the same TeamSpeak channel still hear each
  other directly through the TeamSpeak server. Closing that needs the bridge
  to set the channel's needed talk power and mark who may talk
  (`client_is_talker`), tested against the official server in CI.
- Puppets (Gwar users on TeamSpeak) follow the core; moderation from a
  TeamSpeak client is not supported: TeamSpeak users with Gwar roles moderate
  from the Gwar app.

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
- **Who can see it** stays the private channel's access list, shown above the
  overrides, so the whole picture of a channel is on one page.
- **View as role.** An admin previews the channel tree and the chat as a role
  sees it. Later; not needed for the first release.
- Errors say why: "Moderator is above your rank", not "missing permission".

## Moving existing servers

Gwar is pre-release, so there is no full migration, only what keeps an existing
data directory starting: roles get positions in their order of creation (Admin
first, Member last), stored permission names are rewritten by the table above
(`channel_create`, `channel_edit` and `channel_delete` become
`manage_channels`; `token_create` and `invite_create` become `create_invites`;
`member_remove` and `member_nickname` become `manage_members`), and Admin holds
`administrator`. Step 2 adds `connect`, `speak` and `send_messages` to
Member once, so existing members keep joining, talking and writing.

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
2. **Overrides**: storage, resolution with shared test vectors, the channel
   permissions UI, and `connect`, `send_messages` and per-channel
   `attach_files`, `manage_messages`, `move_members`, `bypass_password` and
   `manage_channels` enforced.
3. **Voice and TeamSpeak**: `speak` and `mute_members` (`client.voice`) in
   the SFU, TeamSpeak users kept out of channels they may not join; the
   bridge's talk power is the remaining part.
4. **Then the importer**, onto this model.

Each step changes authorization, so each goes to an independent review before
it is merged.

## Decided

- A role's channel access is trusted down the order (2026-10-18): anyone with
  `manage_roles` gives roles below their rank, including the hidden channels
  they open. The stricter "only holders give a role" rule is gone.
