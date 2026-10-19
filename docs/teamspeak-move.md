# Moving a TeamSpeak 3 server to Gwar, step by step

About 15 to 60 minutes of work, most of it waiting for builds and DNS. This is
the how-to; what moves and how is in [teamspeak-import.md](teamspeak-import.md).

After the move, Gwar runs the official TeamSpeak server next to it and bridges
it ([README, "TeamSpeak compatibility"](../README.md#teamspeak-compatibility)):
your people keep joining with their TeamSpeak client at the same address, keep
their roles, and switch to the Gwar app when they want. Nobody has to install
anything on moving day.

The import only reads your TeamSpeak server. Until you switch it off, the old
server keeps working as before, so you can try everything first and go back at
any point.

## Pick a path

| | **A. New host** | **B. Same host** |
| --- | --- | --- |
| When | Gwar runs on another machine (a new VPS, say) | Gwar replaces TeamSpeak on the machine it runs on now |
| Downtime | None: the address switches over | A minute or two: stop TeamSpeak, start Gwar |
| Try it first | Yes, for as long as you like, at a second address | Only a dry run (one free TeamSpeak per machine) |
| Address | Move the DNS record (or SRV) to the new host | Stays as it is |

## Before you start (5 minutes)

- **TeamSpeak 3 server**, 3.13 or close; TeamSpeak 6 cannot be imported yet.
- **The `serveradmin` ServerQuery password.** TeamSpeak shows it once, at its
  first start. Lost it: stop TeamSpeak and start it once with
  `serveradmin_password=<new>` on its command line.
- **A backup of TeamSpeak:** stop it for a moment and copy its directory (or at
  least `ts3server.sqlitedb`). The import does not write to it, but a backup costs
  nothing.
- **The channel passwords you want to keep.** TeamSpeak stores them in a form no
  other server can use, so you type them in (step 3). Channels whose password you
  do not give come over locked with a random one, and you set new ones in Gwar.
- **The Gwar server binary** on the host Gwar will run on: the release archive
  `gwar-server-vX.Y.Z-<arch>.tar.gz` from the
  [releases page](https://github.com/MaciejWlodarski/gwar/releases), or
  `cargo build --release -p gwar-server`. On ARM64 also
  `apt install qemu-user libc6-amd64-cross libstdc++6-amd64-cross` (TeamSpeak
  is x86-64 only).
- **Ports** on the Gwar host: TCP 443 and 80, UDP 9987 (TeamSpeak), UDP 9988
  (Gwar voice), TCP 30033 (TeamSpeak file transfer).
- **Slots:** Gwar's TeamSpeak runs on TeamSpeak's free license: 32 slots,
  non-commercial use only. Gwar app users do not take a slot unless they share
  a channel with TeamSpeak users.

Run every `gwar-server` command below as the system user the server runs as
(the running server takes imports only from that user).

## Path A: a new host

### A1. Start Gwar on the new host (10 minutes)

Use a name of its own for now, such as `gwar.example.com`; the address your
people use moves in A5.

```sh
gwar-server --data-dir data --domain gwar.example.com --acme-email you@example.com \
  --teamspeak --accept-teamspeak-license
```

It prints a one-time **admin token**. Open <https://gwar.app>, connect to
`gwar.example.com` and redeem it from the server menu ("Use token"): you are
now the owner. Stop the server again (Ctrl-C) for the first import.

### A2. Reach the old TeamSpeak's ServerQuery (2 minutes)

ServerQuery sends its password in clear, so go through SSH. On the new host:

```sh
ssh -N -L 20011:127.0.0.1:10011 you@old-teamspeak-host &
```

Leave it running. The import then connects to `127.0.0.1:20011` (not 10011:
that port is Gwar's own TeamSpeak's). On the old host the connection comes from
127.0.0.1, which TeamSpeak's `query_ip_allowlist.txt` already trusts, so its
flood protection does not get in the way.

### A3. Dry run and passwords (5 to 15 minutes)

```sh
read -rs GWAR_TS_QUERY_PASSWORD && export GWAR_TS_QUERY_PASSWORD   # not in shell history
gwar-server --data-dir data import-teamspeak --from 127.0.0.1:20011 --dry-run
```

It lists the roles with the permissions they get and why, the channels, the
members and bans, and everything that does not move. Nothing changes. Read it,
then follow the **Next steps** it prints.

Then write the passwords file (also when TeamSpeak has no passwords: the file
is just empty then, and the commands below stay the same):

```sh
gwar-server --data-dir data import-teamspeak --from 127.0.0.1:20011 --passwords-template passwords.txt
```

`passwords.txt` (readable only by you) lists the server's password as
`@server` and every channel with one. Type each password after its `=`, leave a
line empty to have that channel locked, and keep the file private.

### A4. Import (2 minutes)

```sh
gwar-server --data-dir data import-teamspeak --from 127.0.0.1:20011 \
  --passwords passwords.txt --for-bridge --replace-channels
```

- `--for-bridge`: TeamSpeak clients on Gwar's TeamSpeak server use the same
  passwords as before, the server's and the channels'. Gwar keeps them in plain
  text only until its TeamSpeak has taken them (the database is readable by its
  owner only).
- `--replace-channels`: drops Gwar's own starting channels (Lobby, General,
  AFK). It needs the server stopped, so it belongs to the first import only.

Start the server again (the A1 command) and look around in the Gwar app:
the channels, **Server settings → Roles**, and **View as this role** to see the
server as each role does. Connect with a TeamSpeak client to
`gwar.example.com` too. Invite a few people to try it.

From now on you can import again **while the server runs**: the same command
without `--replace-channels`. Everyone connected reconnects by themselves
within seconds. A new import brings over what changed on TeamSpeak and keeps
what you changed in Gwar.

### A5. Moving day (10 minutes, then DNS)

1. A day before, lower the TTL of your TeamSpeak address's DNS record (for
   example to 300 seconds), so the switch takes minutes, not hours.
2. Import once more, with the server running, to bring the latest changes:
   `gwar-server --data-dir data import-teamspeak --from 127.0.0.1:20011 --passwords passwords.txt --for-bridge`.
3. Point the address at the new host: the A record of `ts.example.com`, or the
   `_ts3._udp` SRV record if your people type a name without a port. Add the
   name to Gwar too (another `--domain ts.example.com`) and restart it, so the
   Gwar app can connect there as well.
4. Set a channel description on the old TeamSpeak saying "we moved", and stop
   it after a few days. Close the SSH tunnel.
5. Delete `passwords.txt`.

## Path B: the same host

Only one free-licensed TeamSpeak server may run per machine, so Gwar's
TeamSpeak takes over from the old one: the import runs first, while the old
one still runs, and Gwar starts once the old one has stopped.

### B1. Dry run and passwords (5 to 15 minutes)

Install `gwar-server` next to TeamSpeak, but do not start it yet.

```sh
read -rs GWAR_TS_QUERY_PASSWORD && export GWAR_TS_QUERY_PASSWORD
gwar-server --data-dir data import-teamspeak --from 127.0.0.1:10011 --dry-run
gwar-server --data-dir data import-teamspeak --from 127.0.0.1:10011 --passwords-template passwords.txt
```

Read the report, fill in `passwords.txt` (see A3).

### B2. Import (2 minutes)

```sh
gwar-server --data-dir data import-teamspeak --from 127.0.0.1:10011 \
  --passwords passwords.txt --for-bridge
```

A server that never ran gets the imported channels instead of its starting
ones. It had no database to keep a copy of: to start over, delete
`data/gwar.sqlite3` (and `-wal`, `-shm`) and import again.

### B3. Switch over (2 minutes)

1. Stop the old TeamSpeak (`./ts3server_startscript.sh stop`, or its systemd
   unit) and keep it from starting at boot. Gwar's TeamSpeak needs its ports
   (UDP 9987, TCP 30033).
2. Start Gwar:
   ```sh
   gwar-server --data-dir data --domain ts.example.com --acme-email you@example.com \
     --teamspeak --accept-teamspeak-license
   ```
   The first start downloads the TeamSpeak server and prints a one-time
   **admin token**: redeem it in the Gwar app (A1) to become the owner.
3. Connect with a TeamSpeak client to the usual address, and with the Gwar app
   to `ts.example.com`. Check the channels and roles (see A4).
4. Delete `passwords.txt`.

Going back: stop `gwar-server` and start the old TeamSpeak again. Nothing of it
changed.

## After the move

- **Roles stay with people.** TeamSpeak users keep their roles on Gwar's
  TeamSpeak server: Gwar knows them by their TeamSpeak identity. In the Gwar
  desktop app, someone holding that identity is offered to **claim** it when
  they join, and then has the same roles in the app.
- **Locked channels:** the report lists the channels that came over with a
  random password; set new ones in Gwar (channel settings).
- **Backups:** `gwar-server --data-dir data backup copy.sqlite3` while it runs,
  and keep `data/server-key.ed25519` with it.
- **What did not move** (icons, avatars, channel files, channel descriptions,
  privilege keys; TeamSpeak keeps no chat history): see
  [teamspeak-import.md](teamspeak-import.md#what-moves). Create Gwar invites
  instead of privilege keys.

## When something goes wrong

The importer explains the common failures itself. In short:

| Error | Fix |
| --- | --- |
| Wrong login or password (TeamSpeak error 520) | Check `GWAR_TS_QUERY_PASSWORD` and `--user`; reset with `serveradmin_password=` (above) |
| Banned for flooding (3329, 3331) | Connect through the SSH tunnel (127.0.0.1 is allow-listed), or add the address to `query_ip_allowlist.txt` and restart TeamSpeak |
| No virtual server (1024) | `--sid` with the right id (`serverlist` in ServerQuery) |
| Connection refused, or timed out | Is TeamSpeak running, is the tunnel up, is the query port 10011? |
| "not a TeamSpeak query interface" | Use the raw query port (10011), not 9987 or 10022 |
| "the passwords file names …" | A name several channels share, or one that is not a password channel: write the channel's path, `Parent/Child` |
| "needs the server stopped" | `--replace-channels` only with the Gwar server stopped |

Every import into an existing database keeps it as it was
(`data/gwar.sqlite3.before-import-<time>`) and prints the name. To undo such an
import, stop the server, put that file back as `data/gwar.sqlite3` and delete
`gwar.sqlite3-wal` and `-shm`. The first import into a server that never ran
(path B) has nothing to keep: delete `data/gwar.sqlite3*` to start over.
