# Compatibility between versions (design proposal)

Status: **proposal, not built.** Today a client and a server must come from the
same commit, more or less. Admins of self-hosted servers will not update every
time a feature lands, and the desktop app updates itself on its own schedule, so
clients must work with older servers, and servers with older clients, inside a
stated window. This document says how. Stored data is a separate concern:
[data-migrations.md](data-migrations.md).

## Where things stand

- `gwar/4` is checked for equality on both sides: the client gives up when the
  challenge carries another `protocol`, the server answers a `hello` of another
  version with `bad_request`.
- The web client validates every server frame against exact schemas
  (`apps/web/src/net/schema.ts`): `shape` rejects any key it does not know and
  every listed field is required unless marked `optional`; `literal` rejects
  enum values it does not know. A frame that fails is dropped
  (`net/frames.ts`). Unknown event names are dropped too, which is harmless.
- `#117` removed the defaults for fields older servers did not send.
- The server (serde) ignores unknown fields in requests, except in five signed
  structures with `deny_unknown_fields`; an unknown `op` is answered with
  `bad_request` ("malformed request") and the connection stays.
- `ServerInfo.version` already carries the server's version string, and `hello`
  carries the client's name, version and platform.

### What breaks first

With a client and a server one version apart, these fail first:

1. **A new required field in `ServerInfo`.** `screen_guest_links` (#143) is
   required by the client and inside the `challenge`, the first frame. Today's
   client drops the challenge of any server from before #143 (v0.1.2 included)
   and the connection times out without saying why. Any new field in
   `ServerInfo`, `Welcome`, `Channel`, `Member`, `Client`, `Group` or a response
   does the same to a newer client on an older server.
2. **A new field the server sends to an older client.** The older client's
   `shape` rejects the unknown key and drops the whole frame: a new field in
   `Welcome` loses the welcome (no connection), in an event loses the event, in a
   response makes the request time out.
3. **A new enum value.** A new `Permission`, `ErrorCode`, `Platform`,
   `LeaveReason`, `BadgeKind`, screen preset or surface fails `literal` and drops
   the frame that carries it (for `Permission`: the welcome and every role event).
4. **A new `op`.** An older server answers `bad_request`; the client shows a
   generic failure instead of hiding the feature.
5. **A protocol bump.** Any `PROTOCOL_VERSION` change disconnects everything
   older at once.
6. **E2E formats.** Signed statements carry a version in their first line
   (`gwar conversation v1`, `gwar chat ciphertext v1`, `gwar history chunk v1`,
   `gwar credential v1`, `gwar owner v1`, `gwar managers v1`, `gwar channel members v1`,
   the archive's `… v1` headers) and verifiers accept exactly that line; the chat
   plaintext context is `deny_unknown_fields`; the access extension is `0xff01`.
   A device that writes a v2 of any of them makes every older member fail to
   read it, and in MLS a failed commit can stall the group.

## Goals

- A client works with every server inside the **support window**, hiding what
  the server cannot do and saying clearly when a server is too old.
- A server works with every client inside its window and refuses older ones with
  a clear message, never a silent drop.
- Encrypted conversations keep working while members run different versions in
  the window; a new format is used only when every member can read it.

## Versions and capabilities

**Protocol major.** `protocol` (today 4) changes only for a break that cannot be
done additively. Clients keep a list of majors they speak (`[4]`, later `[4, 5]`),
the server one; within a major everything is additive.

**The bootstrap is frozen.** Every client parses the `challenge` before it sends
`hello`, so the challenge is the one frame the server cannot shape for the
client: it does not know the client yet. Its shape (`protocol`, `server_pk`,
`server_nonce`, `password_required` and `server`, a `ServerInfo` as the oldest
client in the window reads it) is therefore frozen: servers send no new field in the challenge or the `ServerInfo` it
carries, and keep sending every field of today's shape (`screen_guest_links`
included), until every client in the support window parses it leniently (step 1
of the migration path, then one full window). Clients may read it more leniently
at any time, which is what lets them reach servers older than the freeze. Until then,
`ServerInfo` in the challenge is a separate, frozen wire type from the one in
`welcome` and `server.updated`, which may grow per session. The only things a client learns before signing in are
`protocol` and `server.version`, both already in today's challenge, and both are
enough for the decisions it has to take there (wrong major, server too old).

**Features.** Within a major, what a server can do is a list of named features.
They are negotiated after the challenge: the client sends the features it
understands in `hello.client.features`, and the server answers in `welcome`
with the features both sides have:

```json
"server": { "name": "…", "version": "0.3.1", "features": ["screen.share", "screen.guest_links", "roles.overrides", "ts.badges", "mls.private"] }
```

A client that sends no `features` (every client before step 2) gets the welcome
in the baseline shape it was built for, without `features` and without fields of
later features.

- A feature name is a stable, never-reused string for an op, an event or a field
  family (`screen.share` = the `screen.*` ops and events and `Welcome.shares`).
- Every op and field added from now on belongs to one feature. The client
  checks `server.has("screen.share")` before showing the UI or sending the op;
  absent features are hidden, not greyed with an error. Fields of an absent
  feature are treated as their empty value.
- The server shapes the welcome, events and responses of the session from the
  client's list (see "The reverse direction").
- Features never go away within a major; removing one is a major change.

`ServerInfo.version` (the release, `MAJOR.MINOR.PATCH`) is for display and for
the support window, never for feature checks.

## Rules for changing the protocol

1. **Additive only.** A new field is optional on the wire: `#[serde(default,
   skip_serializing_if = …)]` on the server, `optional(…)` in `schema.ts`. A
   field becomes required in the client only when every server in the support
   window sends it.
2. **Ignore unknown fields** in frames from the server. `shape` keeps checking
   the fields it knows (types, bounds) and the prototype-pollution keys, keeps
   the 2 MiB cap and the per-array bounds, but drops unknown keys instead of
   rejecting the frame. Unknown keys are counted for diagnostics. Strictness
   stays where it protects something: signed statements, the challenge
   signature inputs and stored formats (versioned on their own).
3. **Unknown enum values degrade, never reject.** Each enum gets a fallback in
   the client: an unknown `Permission` is ignored in lists (it grants nothing the
   client would show), an unknown `ErrorCode` is a generic error with the
   server's message, an unknown `Platform`/`BadgeKind`/preset/surface shows as
   "other". `literal` gains a lenient twin used for server data.
4. **Never repurpose a field or a value.** A field whose meaning changes gets a
   new name; the old one is still sent while clients in the window read it.
5. **A new op instead of a changed op.** Changing what an op takes or returns
   means a new op name (`channel.create2`, or better a descriptive one) behind a
   feature; the old op stays for older clients.
6. **Servers answer unknown ops with `unsupported`** (a new `ErrorCode`), which
   clients map to "this server cannot do that" without retrying.
7. **Responses grow like events:** new fields optional, old ones kept.
8. **Server and Connect databases** are internal to one process and migrate on
   their own ([data-migrations.md](data-migrations.md)); only the wire is shared.
   What the server *sends* from a new column follows the rules above.

`schema.ts` stays the single place that knows every type: each rule is generated
or checked against the Rust types as today; the change is the lenient `shape`
and `literal` for server frames, and `optional` on every field newer than the
oldest supported server.

## The support window

- **Clients support servers of the last two minor releases** (for example a
  0.5 client: 0.4.x and 0.5.x), or six months, whichever is longer. The
  minimum is a constant in the client (`MIN_SERVER_VERSION`), raised on purpose
  in a release note.
- The client reads `server.version` from the challenge (a field every server
  sends today, so this check works against servers older than the check
  itself). Below the minimum it stops before signing in and shows: "This server runs Gwar 0.3.2. This app needs
  0.4 or later: ask the server's admin to update." with the server's name. The
  connect screen keeps the bookmark.
- Inside the window but older than the client: the client connects, hides
  missing features, and the server info panel says "This server runs an older
  Gwar (0.4.1): some features are not available."
- A server newer than the client (the app not updated yet): connects; features
  the client does not know are simply not shown. If the server's protocol major
  is newer, the client says "Update Gwar to join this server".

## The reverse direction: an old client on a new server

- The server knows the client from `hello.client` (`name`, `version`,
  `platform`, and from now on `features`). It keeps a minimum client version
  (`--min-client-version`, default: the oldest client release still in its
  window) and answers older ones with a new `client_too_old` error whose message
  names the version to install; the client shows it as such.
- The challenge is sent before the server knows the client, so it stays in its
  frozen baseline shape (see "Versions and capabilities"); an older client always
  parses it and gets as far as `hello`.
- Until every client in the window has the lenient parser (step 1 below), the
  server must not add fields to existing frames for clients that did not
  advertise the feature: from `welcome` on it sends per-session frames shaped for
  the client's feature list. After that, only new ops and events need gating, and
  the server never sends events of a feature the client did not advertise.
- `client_too_old` is an answer to `hello`, so it reaches even a strict older
  client: `ErrorCode` gets the value in the release that adds the parser for it,
  and until every client in the window knows it the server sends it as
  `bad_request` with the same message, which today's clients already show.
- TeamSpeak clients are unaffected: the bridge speaks TeamSpeak's protocol.
- The web app a server serves itself (`--web-root`) matches that server; the web
  app at gwar.app is always the newest, so it is the client that meets the
  oldest servers.

## E2E and MLS formats

MLS state is shared between devices of different versions, so formats change in
two phases: **read everywhere first, write later**.

- **Statements and ciphertext contexts** keep their versioned first line. A
  verifier accepts every version it knows; a writer writes the newest version
  every reader can read. A new version is added to readers one release before
  any writer uses it.
- **What a group can use** is decided by its members, through MLS capabilities:
  each device's leaf node lists the Gwar extensions and formats it supports
  (custom extension types next to `0xff01`, and a `gwar formats` capability
  listing e.g. `chat v1, chat v2`). A writer uses a format only if every current
  member lists it; a group may require one with `RequiredCapabilities` once every
  device in the window supports it, after which older devices cannot join (they
  get a clear "update to read this channel").
- **Descriptors** (`gwar conversation v1`) bind the group id, so a new descriptor
  version applies only to new incarnations: an existing group keeps its
  descriptor, a restart may move to the new version when all members read it.
- **Access extension (`0xff01`)** changes get a new extension type rather than a
  new layout under the same number; groups switch at a restart.
- **The chat plaintext** gains a version field before it gains anything else;
  `deny_unknown_fields` stays for its versions (a new version is a new shape).
- **History chunks and the key archive** carry versions already; the archive's
  records are read by every device of the account, so the same read-first rule
  applies across the account's devices.

## Migration path from today

1. **Lenient reading (clients first).** Make `shape` drop unknown keys and add the
   enum fallbacks for server frames; make every field added since v0.1.2 that a
   server in the window may lack `optional` (first: `ServerInfo.screen_guest_links`,
   so today's client works with a v0.1.2 server). Report "server version too old"
   from the challenge's `server.version` instead of timing out. Add `unsupported`
   to `ErrorCode`. Freeze the challenge: split its `ServerInfo` from the
   welcome's and pin its shape with a test, so no PR can change it by accident.
2. **Features.** Add `features` to `hello.client` and to the welcome's
   `ServerInfo` (never the challenge's); the client's `has(feature)` gates the
   screen sharing, roles override, TeamSpeak badge, private channel and
   watch-link UI first, as the newest features. Name every feature from then on
   in the PR that adds it.
3. **The window.** `MIN_SERVER_VERSION` in clients and `--min-client-version` in
   the server, with the messages above; `client_too_old` error.
4. **CI.** A job that runs the browser smoke suite against the previous release's
   server, and the previous release's desktop/web client against the current
   server, so a break shows up in the PR that causes it.
5. **E2E versions.** Gwar format capabilities in leaf nodes, readers of several
   versions, and the read-first rule in the e2e docs, before the first v2 format.
6. **Major 5** only if something cannot be additive; clients by then speak 4 and
   5 for a window. Changing the challenge itself counts as a major change until
   one window after step 1, when every supported client reads it leniently.
