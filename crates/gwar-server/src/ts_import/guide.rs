//! What the importer tells the admin besides the report: how to get past a failed ServerQuery
//! connection, and what to do after a dry run or an import (docs/teamspeak-move.md).

use std::fmt::Write;

use super::plan::{Password, Plan};
use crate::teamspeak::query::QueryError;

/// A hint for an error reaching or reading the TeamSpeak server, if it is one we know.
pub fn query_hint(error: &anyhow::Error, from: &str, sid: u32) -> Option<String> {
    if let Some(e) = error.chain().find_map(|c| c.downcast_ref::<QueryError>()) {
        return Some(match e.id {
            520 => "Wrong ServerQuery login or password. TeamSpeak shows the serveradmin password once, at its \
                    first start (on the console and in its first log under logs/). Lost it: stop TeamSpeak \
                    and start it once with serveradmin_password=<new> on its command line."
                .into(),
            3331 => "TeamSpeak has banned this address for too many queries (flood protection, 10 minutes \
                     by default). Add the address the import connects from to query_ip_allowlist.txt \
                     next to ts3server (127.0.0.1 is there by default, so an SSH tunnel avoids this), \
                     restart TeamSpeak, and run the import again."
                .into(),
            3329 => "TeamSpeak refuses this address: it is banned there (by a ban, or for flooding). Lift \
                     the ban on TeamSpeak, or connect from an address in its query_ip_allowlist.txt \
                     (127.0.0.1, through an SSH tunnel)."
                .into(),
            1024 => format!(
                "TeamSpeak has no virtual server {sid}. Most have only server 1; `serverlist` in a \
                 ServerQuery session lists them. Pass the right one with --sid."
            ),
            2568 => "This query login may not read everything the import needs: log in as serveradmin \
                     (--user serveradmin)."
                .into(),
            _ => return None,
        });
    }
    let refused = error
        .chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|e| e.kind() == std::io::ErrorKind::ConnectionRefused);
    if refused {
        return Some(format!(
            "Nothing answers on {from}. Is TeamSpeak running, and is its ServerQuery on that port (10011 \
             unless query_port says otherwise)? For a TeamSpeak server on another machine, open a tunnel \
             first: ssh -N -L 20011:127.0.0.1:10011 user@that-host, then --from 127.0.0.1:20011 (not 10011 \
             locally: a Gwar server with --teamspeak uses it for its own TeamSpeak)."
        ));
    }
    let text = format!("{error:#}");
    if text.contains("not a TeamSpeak query interface") {
        return Some(format!(
            "{from} is not TeamSpeak's raw ServerQuery: use its query port (10011 by default), not the voice \
             port (9987) or the SSH query port (10022)."
        ));
    }
    if text.contains("timed out") {
        return Some(format!(
            "{from} did not answer in time: a firewall may drop the connection. Reach TeamSpeak through an \
             SSH tunnel (ssh -N -L 20011:127.0.0.1:10011 user@that-host) and use --from 127.0.0.1:20011."
        ));
    }
    None
}

/// What the dry run means for the next step.
pub struct DryRun<'a> {
    pub plan: &'a Plan,
    /// --passwords was given.
    pub passwords_given: bool,
    pub drop_passwords: bool,
    pub for_bridge: bool,
    pub replace_channels: bool,
    /// The Gwar server holds its data directory (runs): the import would go through it.
    pub server_running: bool,
}

fn locked(plan: &Plan) -> usize {
    let channels = plan.channels.iter().filter(|c| c.password == Password::Locked).count();
    channels + usize::from(plan.server_password == Password::Locked)
}

fn kept(plan: &Plan) -> usize {
    let channels = plan.channels.iter().filter(|c| matches!(c.password, Password::Kept(_))).count();
    channels + usize::from(matches!(plan.server_password, Password::Kept(_)))
}

/// "Next steps" after a dry run.
pub fn after_dry_run(d: &DryRun) -> String {
    let mut out = String::from("\nNext steps:\n");
    let mut step = 0;
    let mut next = |out: &mut String, text: &str| {
        step += 1;
        let _ = writeln!(out, "  {step}. {text}");
    };
    let locked = locked(d.plan);
    if locked > 0 && !d.drop_passwords {
        let what = if d.passwords_given { "still have no password given" } else { "have a password" };
        next(
            &mut out,
            &format!(
                "{locked} of TeamSpeak's channels or its server {what}; without one they come over locked \
                 with a random password. Write them all into a file to fill in: add \
                 --passwords-template passwords.txt (it keeps what --passwords already gives), type each \
                 password after its \"=\", then run with --passwords passwords.txt. Or --drop-passwords \
                 imports them without one."
            ),
        );
    }
    if kept(d.plan) > 0 && !d.for_bridge {
        next(
            &mut out,
            "If this Gwar server runs with --teamspeak, add --for-bridge, so TeamSpeak clients on it use the \
             same passwords as before.",
        );
    }
    if d.server_running && d.replace_channels {
        next(
            &mut out,
            "Stop this server, then run the same command without --dry-run: --replace-channels needs it \
             stopped. (Later imports can run while it runs, without --replace-channels.)",
        );
    } else if d.server_running {
        next(
            &mut out,
            "Run the same command without --dry-run. This server is running: the import goes through it, and \
             everyone connected reconnects by themselves within seconds.",
        );
    } else {
        next(&mut out, "Run the same command without --dry-run (this server is stopped: it imports directly).");
    }
    next(&mut out, "The whole move, step by step: docs/teamspeak-move.md.");
    out
}

/// "Next steps" after an import. `backup` is the database as it was, if there was one.
pub fn after_import(plan: &Plan, live: bool, backup: Option<&str>) -> String {
    let mut out = String::from("\nNext steps:\n");
    let mut lines = vec![
        "Look it over in the Gwar app: the channels, Server settings → Roles, and \"View as this role\" \
         to see the server as a role sees it."
            .to_owned(),
        "People keep their TeamSpeak roles: on Gwar's TeamSpeak server with the same identity, and in the \
         desktop app once they claim that identity (it offers to when they join)."
            .to_owned(),
        "Import again just before moving day: it brings over what changed on TeamSpeak since and keeps what \
         you changed in Gwar."
            .to_owned(),
        "Moving day: point your address (DNS, or the _ts3._udp SRV record) at this server; \
         docs/teamspeak-move.md."
            .to_owned(),
    ];
    let locked = locked(plan);
    if locked > 0 {
        lines.insert(
            1,
            format!(
                "{locked} password-protected channels (or the server) came over locked with a random password: \
                 set new ones in Gwar, or import again with --passwords."
            ),
        );
    }
    if let Some(backup) = backup {
        let how = if live { "stop this server, then put" } else { "put" };
        lines.push(format!("To undo it: {how} {backup} back as gwar.sqlite3 (delete gwar.sqlite3-wal and -shm)."));
    }
    for line in lines {
        let _ = writeln!(out, "  - {line}");
    }
    out
}
