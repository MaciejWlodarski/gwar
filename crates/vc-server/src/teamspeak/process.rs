//! Run the official TeamSpeak 3 server as a hidden, supervised child process.

use std::{
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    net::TcpStream,
    process::{Child, Command},
    sync::{oneshot, watch},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};

use super::install::Installed;

/// Under emulation the server precomputes a puzzle for several seconds before it listens.
const READY_TIMEOUT: Duration = Duration::from_secs(90);
const STOP_GRACE: Duration = Duration::from_secs(5);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const HEALTHY_AFTER: Duration = Duration::from_secs(60);
const TAIL_LINES: usize = 12;

#[derive(Clone)]
pub struct ProcessConfig {
    pub installed: Installed,
    /// Working directory: database, logs and the query allowlist live here and survive upgrades.
    pub state_dir: PathBuf,
    /// Public UDP voice bind, e.g. `0.0.0.0:9987`.
    pub voice: SocketAddr,
    /// Raw ServerQuery port, bound to 127.0.0.1 only.
    pub query_port: u16,
    /// TCP file transfer bind, default `127.0.0.1:30033`.
    pub filetransfer: SocketAddr,
    /// `serveradmin` query password.
    pub admin_password: String,
}

/// A supervised `ts3server`; dropping it stops the server (SIGTERM, then kill after a grace period).
pub struct TsProcess {
    generation: watch::Receiver<u64>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl TsProcess {
    /// Start the server and return once its query port accepts connections.
    pub async fn spawn(config: ProcessConfig) -> Result<Self> {
        prepare_state(&config).await?;
        let (gen_tx, generation) = watch::channel(0);
        let (stop, stop_rx) = oneshot::channel();
        let (first_tx, first_rx) = oneshot::channel();
        let task = tokio::spawn(supervise(config, gen_tx, stop_rx, first_tx));
        let mut this = TsProcess { generation, stop: Some(stop), task: Some(task) };
        match first_rx.await {
            Ok(Ok(())) => Ok(this),
            Ok(Err(e)) => {
                this.shutdown().await;
                Err(e)
            }
            Err(_) => bail!("the TeamSpeak supervisor stopped unexpectedly"),
        }
    }

    /// Bumped each time the server (re)became ready, so callers can reconnect their query session.
    pub fn generation(&self) -> watch::Receiver<u64> {
        self.generation.clone()
    }

    /// Stop the server and wait until it has exited.
    pub async fn shutdown(&mut self) {
        self.stop.take();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for TsProcess {
    fn drop(&mut self) {
        // Closing the channel tells the supervisor to stop the child.
        self.stop.take();
    }
}

async fn prepare_state(config: &ProcessConfig) -> Result<()> {
    let state = &config.state_dir;
    tokio::fs::create_dir_all(state.join("logs")).await.with_context(|| format!("create {}", state.display()))?;
    // The query port is loopback-only, but the allowlist also exempts us from
    // flood and brute-force bans, including our puppets, which connect from
    // the voice address when it is not a wildcard.
    let mut allow = String::from("127.0.0.1\n::1\n");
    if !config.voice.ip().is_unspecified() && !config.voice.ip().is_loopback() {
        allow.push_str(&format!("{}\n", config.voice.ip()));
    }
    tokio::fs::write(state.join("query_ip_allowlist.txt"), allow).await?;
    Ok(())
}

fn server_binary(dir: &Path) -> PathBuf {
    dir.join(if cfg!(windows) { "ts3server.exe" } else { "ts3server" })
}

fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// The program and leading arguments that run the x86-64 binary on this machine.
fn launcher(installed: &Installed) -> Result<(PathBuf, Vec<String>)> {
    let binary = server_binary(&installed.dir);
    if !installed.emulated {
        return Ok((binary, Vec::new()));
    }
    let binary = binary.to_string_lossy().into_owned();
    if let Some(box64) = which("box64") {
        return Ok((box64, vec![binary]));
    }
    if let Some(qemu) = which("qemu-x86_64") {
        let sysroot = Path::new("/usr/x86_64-linux-gnu");
        if sysroot.is_dir() {
            return Ok((qemu, vec!["-L".into(), sysroot.display().to_string(), binary]));
        }
    }
    bail!(
        "running the x86-64 TeamSpeak server on this CPU needs box64 or qemu-user; install it with \
         `sudo apt install qemu-user libc6-amd64-cross libstdc++6-amd64-cross`"
    )
}

fn arguments(config: &ProcessConfig) -> Vec<String> {
    let state = &config.state_dir;
    let sql = config.installed.dir.join("sql");
    let mut sql = sql.display().to_string();
    sql.push(std::path::MAIN_SEPARATOR);
    vec![
        "license_accepted=1".into(),
        format!("default_voice_port={}", config.voice.port()),
        format!("voice_ip={}", config.voice.ip()),
        format!("query_ip={}", Ipv4Addr::LOCALHOST),
        format!("query_port={}", config.query_port),
        "query_protocols=raw".into(),
        format!("filetransfer_ip={}", config.filetransfer.ip()),
        format!("filetransfer_port={}", config.filetransfer.port()),
        format!("serveradmin_password={}", config.admin_password),
        format!("logpath={}", state.join("logs").display()),
        format!("dbsqlpath={sql}"),
        format!("query_ip_allowlist={}", state.join("query_ip_allowlist.txt").display()),
        "create_default_virtualserver=1".into(),
    ]
}

fn command(config: &ProcessConfig) -> Result<Command> {
    let (program, mut args) = launcher(&config.installed)?;
    args.extend(arguments(config));
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(&config.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let libs = &config.installed.dir;
    if cfg!(target_os = "linux") {
        cmd.env("LD_LIBRARY_PATH", libs);
    } else if cfg!(target_os = "macos") {
        cmd.env("DYLD_LIBRARY_PATH", libs);
    }
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: no console window for the hidden server.
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: prctl is async-signal-safe; nothing else runs between fork and exec.
    unsafe {
        // Stop the server even if we die without running destructors (SIGKILL,
        // crash): an orphan would hold the host's single free-license slot.
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(cmd)
}

type Tail = Arc<Mutex<VecDeque<String>>>;

/// Forward child output to `tracing` and remember the last few lines for error reports.
fn forward_output(stream: impl AsyncRead + Unpin + Send + 'static, tail: Tail) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("|ERROR") || line.contains("|WARNING") {
                tracing::warn!(target: "teamspeak::server", "{line}");
            } else {
                tracing::debug!(target: "teamspeak::server", "{line}");
            }
            let mut tail = tail.lock().unwrap();
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    });
}

fn tail_text(tail: &Tail) -> String {
    let tail = tail.lock().unwrap();
    if tail.is_empty() {
        String::new()
    } else {
        format!("; last output: {}", tail.iter().cloned().collect::<Vec<_>>().join(" / "))
    }
}

/// Resolves once the query port speaks the ServerQuery banner.
async fn wait_ready(port: u16) {
    loop {
        if let Ok(Ok(stream)) = timeout(Duration::from_secs(2), TcpStream::connect((Ipv4Addr::LOCALHOST, port))).await {
            let mut line = String::new();
            let mut reader = BufReader::new(stream);
            if let Ok(Ok(n)) = timeout(Duration::from_secs(2), reader.read_line(&mut line)).await
                && n > 0
                && line.starts_with("TS3")
            {
                return;
            }
        }
        sleep(Duration::from_millis(250)).await;
    }
}

/// Ask the child to exit, killing it after the grace period.
async fn terminate(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: plain signal delivery to a child we spawned and have not yet reaped.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if timeout(STOP_GRACE, child.wait()).await.is_ok() {
            return;
        }
        tracing::warn!(target: "teamspeak::server", "no exit within {STOP_GRACE:?} of SIGTERM, killing");
    }
    let _ = child.kill().await;
}

enum Outcome {
    Stopped,
    Exited(String),
}

/// One child lifetime: spawn, wait for readiness, run until it exits or we are told to stop.
async fn run_once(
    config: &ProcessConfig,
    gen_tx: &watch::Sender<u64>,
    stop: &mut oneshot::Receiver<()>,
    first: &mut Option<oneshot::Sender<Result<()>>>,
    ready_seen: &mut bool,
) -> Result<Outcome> {
    let mut child = command(config)?.spawn().with_context(|| "start ts3server")?;
    let tail = Tail::default();
    if let Some(out) = child.stdout.take() {
        forward_output(out, tail.clone());
    }
    if let Some(err) = child.stderr.take() {
        forward_output(err, tail.clone());
    }
    tracing::info!(target: "teamspeak::server", "started ts3server (pid {:?})", child.id());
    let deadline = sleep(READY_TIMEOUT);
    tokio::pin!(deadline);
    let ready = wait_ready(config.query_port);
    tokio::pin!(ready);
    let mut is_ready = false;
    loop {
        tokio::select! {
            _ = &mut *stop => {
                terminate(&mut child).await;
                return Ok(Outcome::Stopped);
            }
            status = child.wait() => {
                // Let the output readers drain so the tail is complete.
                sleep(Duration::from_millis(100)).await;
                let status = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                return Ok(Outcome::Exited(format!("{status}{}", tail_text(&tail))));
            }
            () = &mut ready, if !is_ready => {
                is_ready = true;
                *ready_seen = true;
                gen_tx.send_modify(|g| *g += 1);
                tracing::info!(target: "teamspeak::server", "ts3server is ready");
                if let Some(first) = first.take() {
                    let _ = first.send(Ok(()));
                }
            }
            () = &mut deadline, if !is_ready => {
                terminate(&mut child).await;
                return Ok(Outcome::Exited(format!("query port not ready after {READY_TIMEOUT:?}{}", tail_text(&tail))));
            }
        }
    }
}

async fn supervise(
    config: ProcessConfig,
    gen_tx: watch::Sender<u64>,
    mut stop: oneshot::Receiver<()>,
    first: oneshot::Sender<Result<()>>,
) {
    let mut first = Some(first);
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = Instant::now();
        let mut ready_seen = false;
        let reason = match run_once(&config, &gen_tx, &mut stop, &mut first, &mut ready_seen).await {
            Ok(Outcome::Stopped) => {
                tracing::info!(target: "teamspeak::server", "ts3server stopped");
                return;
            }
            Ok(Outcome::Exited(reason)) => reason,
            Err(e) => format!("{e:#}"),
        };
        if let Some(first) = first.take() {
            // The very first start failed: a bad configuration (port in use, missing emulator) will not heal.
            let _ = first.send(Err(anyhow!("ts3server failed to start: {reason}")));
            return;
        }
        if ready_seen && started.elapsed() >= HEALTHY_AFTER {
            backoff = BACKOFF_MIN;
        }
        tracing::warn!(target: "teamspeak::server", "ts3server exited ({reason}); restarting in {backoff:?}");
        tokio::select! {
            _ = &mut stop => return,
            () = sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(emulated: bool) -> ProcessConfig {
        ProcessConfig {
            installed: Installed { dir: PathBuf::from("/opt/ts/server-3.13.7"), emulated },
            state_dir: PathBuf::from("/var/ts"),
            voice: "0.0.0.0:9987".parse().unwrap(),
            query_port: 10011,
            filetransfer: "127.0.0.1:30033".parse().unwrap(),
            admin_password: "pw".into(),
        }
    }

    #[test]
    fn command_line() {
        let args = arguments(&config(false));
        for expected in [
            "license_accepted=1",
            "default_voice_port=9987",
            "voice_ip=0.0.0.0",
            "query_ip=127.0.0.1",
            "query_port=10011",
            "query_protocols=raw",
            "filetransfer_ip=127.0.0.1",
            "filetransfer_port=30033",
            "serveradmin_password=pw",
            "logpath=/var/ts/logs",
            "dbsqlpath=/opt/ts/server-3.13.7/sql/",
            "query_ip_allowlist=/var/ts/query_ip_allowlist.txt",
            "create_default_virtualserver=1",
        ] {
            assert!(args.iter().any(|a| a == expected), "missing {expected}: {args:?}");
        }
    }

    #[test]
    fn native_launcher_runs_the_binary_directly() {
        let (program, args) = launcher(&config(false).installed).unwrap();
        assert_eq!(program, server_binary(Path::new("/opt/ts/server-3.13.7")));
        assert!(args.is_empty());
    }
}
