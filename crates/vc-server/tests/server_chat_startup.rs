//! Permanent server-chat mode is chosen by serving, never by maintenance.

use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use vc_server::store::Store;

fn cli(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vc-server"));
    command.env_clear().arg("--data-dir").arg(dir);
    command
}

fn mode(dir: &Path) -> Option<String> {
    Store::open(&dir.join("vc.sqlite3")).unwrap().meta("server_chat").unwrap()
}

#[test]
fn administrative_commands_leave_a_new_database_mode_unset() {
    for args in [
        vec!["server-key"],
        vec!["admin-token"],
        vec!["members", "list"],
        vec!["members", "prune", "--inactive-days", "1", "--dry-run"],
        vec!["members", "remove", "missing", "--dry-run"],
        vec!["members", "merge", "missing", "also-missing", "--dry-run"],
        vec!["backup", "backup.sqlite3"],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut command = cli(dir.path());
        command.current_dir(dir.path()).arg("--encrypted-server-chat").args(&args);
        let output = command.output().unwrap();
        if !args.contains(&"missing") {
            assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
        }
        assert_eq!(mode(dir.path()), None, "{args:?}");
        assert_eq!(mode(dir.path()), None, "reopening after {args:?} must not choose a mode");
        if args[0] == "backup" {
            assert_eq!(Store::open(&dir.path().join("backup.sqlite3")).unwrap().meta("server_chat").unwrap(), None);
        }
    }
}

struct Serving(Child);

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(dir: &Path, flag: bool, env: Option<&str>) -> String {
    let mut command = cli(dir);
    command.args([
        "--http",
        "127.0.0.1:0",
        "--media",
        "127.0.0.1:0",
        "--public-ip",
        "127.0.0.1",
        "--no-connect",
        "--insecure-any-host",
    ]);
    if flag {
        command.arg("--encrypted-server-chat");
    }
    if let Some(value) = env {
        command.env("VC_ENCRYPTED_SERVER_CHAT", value);
    }
    let mut serving = Serving(command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap());
    let logs = Arc::new(Mutex::new(String::new()));
    let mut readers = Vec::new();
    let pipes: Vec<Box<dyn std::io::Read + Send>> =
        vec![Box::new(serving.0.stdout.take().unwrap()), Box::new(serving.0.stderr.take().unwrap())];
    for pipe in pipes {
        let logs = logs.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                let line = line.unwrap();
                let mut logs = logs.lock().unwrap();
                logs.push_str(&line);
                logs.push('\n');
            }
        }));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if logs.lock().unwrap().contains("server ready") {
            break;
        }
        assert!(serving.0.try_wait().unwrap().is_none(), "server exited: {}", logs.lock().unwrap());
        assert!(Instant::now() < deadline, "server did not start: {}", logs.lock().unwrap());
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(serving);
    for reader in readers {
        reader.join().unwrap();
    }
    Arc::try_unwrap(logs).unwrap().into_inner().unwrap()
}

#[test]
fn first_serving_start_chooses_the_mode_from_the_flag_or_boolish_environment() {
    for (flag, env, expected) in [
        (false, None, "plaintext"),
        (true, None, "encrypted"),
        (false, Some("yes"), "encrypted"),
        (false, Some("off"), "plaintext"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        assert!(cli(dir.path()).arg("admin-token").output().unwrap().status.success());
        assert_eq!(mode(dir.path()), None);
        serve(dir.path(), flag, env);
        assert_eq!(mode(dir.path()).as_deref(), Some(expected));
    }
}

#[test]
fn serving_keeps_the_stored_mode_and_warns_only_on_a_conflicting_flag() {
    for encrypted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        serve(dir.path(), encrypted, None);
        let logs = serve(dir.path(), !encrypted, None);
        assert_eq!(mode(dir.path()).as_deref(), Some(if encrypted { "encrypted" } else { "plaintext" }));
        assert_eq!(logs.contains("keeping stored plaintext server chat"), !encrypted, "{logs}");
    }
}
