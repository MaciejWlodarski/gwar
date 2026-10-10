//! Client for the TeamSpeak raw ServerQuery protocol (line-based text over TCP).
//!
//! One task owns the socket and serializes commands; `notify*` lines are routed to a separate channel and never
//! mistaken for a reply, even when they arrive between a command and its `error` line.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, interval_at, sleep, timeout},
};

/// One key/value group of a reply; values are unescaped and bare flags map to `""`.
pub type Record = BTreeMap<String, String>;

const KEEPALIVE: Duration = Duration::from_secs(60);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const NOTIFICATION_BACKLOG: usize = 4096;
const FLOOD_ERROR: u32 = 524;

#[derive(Debug, thiserror::Error)]
#[error("teamspeak query error {id}: {msg}")]
pub struct QueryError {
    pub id: u32,
    pub msg: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// E.g. `notifycliententerview`.
    pub name: String,
    pub records: Vec<Record>,
}

/// Escape a value for the wire.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            ' ' => out.push_str("\\s"),
            '|' => out.push_str("\\p"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            c => out.push(c),
        }
    }
    out
}

/// Inverse of [`escape`]; unknown escapes keep the escaped character.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('p') => out.push('|'),
            Some('a') => out.push('\x07'),
            Some('b') => out.push('\x08'),
            Some('f') => out.push('\x0c'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('v') => out.push('\x0b'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Command builder: `Cmd::new("clientkick").arg("clid", 5).flag("-uid")`.
#[derive(Debug, Clone)]
pub struct Cmd {
    name: String,
    /// `|`-separated groups of `key=value` pairs.
    records: Vec<Vec<(String, String)>>,
    flags: Vec<String>,
}

impl Cmd {
    pub fn new(name: &str) -> Self {
        Cmd { name: name.to_owned(), records: vec![Vec::new()], flags: Vec::new() }
    }

    pub fn arg(mut self, key: &str, value: impl ToString) -> Self {
        self.records.last_mut().expect("at least one record").push((key.to_owned(), value.to_string()));
        self
    }

    /// Option such as `-uid`; the leading dash is added when missing.
    pub fn flag(mut self, flag: &str) -> Self {
        self.flags.push(if flag.starts_with('-') { flag.to_owned() } else { format!("-{flag}") });
        self
    }

    /// Start a new `|`-separated group (for commands taking several records).
    pub fn record(mut self) -> Self {
        self.records.push(Vec::new());
        self
    }

    fn line(&self) -> String {
        let mut line = self.name.clone();
        for (i, record) in self.records.iter().enumerate() {
            if i > 0 {
                line.push('|');
            } else if !record.is_empty() {
                line.push(' ');
            }
            let pairs: Vec<String> = record.iter().map(|(k, v)| format!("{k}={}", escape(v))).collect();
            line.push_str(&pairs.join(" "));
        }
        for flag in &self.flags {
            line.push(' ');
            line.push_str(flag);
        }
        line
    }
}

fn parse_records(s: &str) -> Vec<Record> {
    s.split('|')
        .map(|group| {
            group
                .split(' ')
                .filter(|t| !t.is_empty())
                .map(|token| match token.split_once('=') {
                    Some((k, v)) => (k.to_owned(), unescape(v)),
                    None => (token.to_owned(), String::new()),
                })
                .collect()
        })
        .collect()
}

fn parse_notification(line: &str) -> Notification {
    let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
    Notification { name: name.to_owned(), records: parse_records(rest) }
}

fn parse_error(rest: &str) -> Result<(), QueryError> {
    let record = parse_records(rest).into_iter().next().unwrap_or_default();
    let id = record.get("id").and_then(|v| v.parse().ok()).unwrap_or(u32::MAX);
    if id == 0 {
        return Ok(());
    }
    let mut msg = record.get("msg").cloned().unwrap_or_default();
    if let Some(extra) = record.get("extra_msg").filter(|e| !e.is_empty()) {
        msg = format!("{msg} ({extra})");
    }
    Err(QueryError { id, msg })
}

/// Seconds to back off after a flood-protection error ("please wait N seconds").
fn flood_wait(err: &QueryError) -> Option<Duration> {
    if err.id != FLOOD_ERROR {
        return None;
    }
    let secs = err.msg.split_once("wait ").and_then(|(_, rest)| {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u64>().ok()
    });
    Some(Duration::from_secs(secs.unwrap_or(1).clamp(1, 60)))
}

type Reply = oneshot::Sender<Result<Vec<Record>, QueryError>>;

struct Request {
    line: String,
    /// `None` for internal keepalives.
    reply: Option<Reply>,
}

struct Lines {
    rx: mpsc::Receiver<String>,
    task: JoinHandle<()>,
}

impl Drop for Lines {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Connection {
    tx: mpsc::Sender<Request>,
    task: JoinHandle<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
pub struct Query {
    connection: Arc<Connection>,
}

impl Query {
    /// Connect, log in and select virtual server `server_id`. The notification receiver closes when the connection
    /// dies. Dropping the last query handle closes the socket and its reader task.
    pub async fn connect(
        addr: SocketAddr,
        login: &str,
        password: &str,
        server_id: u32,
    ) -> Result<(Query, mpsc::Receiver<Notification>)> {
        let stream = timeout(Duration::from_secs(10), TcpStream::connect(addr))
            .await
            .context("connect timed out")?
            .with_context(|| format!("connect to {addr}"))?;
        stream.set_nodelay(true).ok();
        let (read, write) = stream.into_split();
        let (line_tx, line_rx) = mpsc::channel(256);
        let task = tokio::spawn(read_lines(read, line_tx));
        let mut lines = Lines { rx: line_rx, task };
        // Banner: "TS3" plus a welcome line.
        for expected in 0..2 {
            let line = timeout(Duration::from_secs(10), lines.rx.recv())
                .await
                .context("timed out waiting for the query banner")?
                .context("connection closed before the banner")?;
            if expected == 0 && !line.starts_with("TS3") {
                bail!("not a TeamSpeak query interface (banner {line:?})");
            }
        }
        let (tx, rx) = mpsc::channel(32);
        let (notify_tx, notify_rx) = mpsc::channel(NOTIFICATION_BACKLOG);
        let task = tokio::spawn(run(write, lines, rx, notify_tx));
        let query = Query { connection: Arc::new(Connection { tx, task }) };
        query
            .call(Cmd::new("login").arg("client_login_name", login).arg("client_login_password", password))
            .await
            .context("query login")?;
        query.call(Cmd::new("use").arg("sid", server_id)).await.context("select virtual server")?;
        Ok((query, notify_rx))
    }

    /// A dedicated query client stays in `cid` to hear that channel's text.
    pub(super) async fn listen_channel(
        addr: SocketAddr,
        password: &str,
        cid: u64,
    ) -> Result<(Query, mpsc::Receiver<Notification>)> {
        let (query, notes) = Self::connect(addr, "serveradmin", password, 1).await?;
        query.call(Cmd::new("clientupdate").arg("client_nickname", format!("Gwar chat {cid}"))).await?;
        let me = query.call(Cmd::new("whoami")).await?;
        let clid = me.first().and_then(|r| r.get("client_id")).context("whoami returned no client id")?;
        if let Err(e) = query.call(Cmd::new("clientmove").arg("clid", clid).arg("cid", cid)).await
            && e.downcast_ref::<QueryError>().map(|e| e.id) != Some(770)
        {
            return Err(e.context("move channel listener"));
        }
        query.call(Cmd::new("servernotifyregister").arg("event", "textchannel")).await?;
        Ok((query, notes))
    }

    /// Run a command. A non-zero error id comes back as a [`QueryError`] inside the `anyhow::Error`.
    pub async fn call(&self, cmd: Cmd) -> Result<Vec<Record>> {
        let line = cmd.line();
        match self.send(&line).await {
            Err(e) => match e.downcast_ref::<QueryError>().and_then(flood_wait) {
                Some(wait) => {
                    tracing::warn!(target: "teamspeak::query", "flood protection, retrying in {wait:?}");
                    sleep(wait + Duration::from_millis(250)).await;
                    self.send(&line).await
                }
                None => Err(e),
            },
            ok => ok,
        }
    }

    async fn send(&self, line: &str) -> Result<Vec<Record>> {
        let (reply, rx) = oneshot::channel();
        self.connection
            .tx
            .send(Request { line: line.to_owned(), reply: Some(reply) })
            .await
            .map_err(|_| anyhow::anyhow!("query connection closed"))?;
        match timeout(CALL_TIMEOUT, rx).await {
            Err(_) => bail!("query command timed out"),
            Ok(Err(_)) => bail!("query connection closed"),
            Ok(Ok(result)) => Ok(result?),
        }
    }
}

/// Forward lines from the socket, tolerating `\n\r`, `\r\n` and `\n` terminators.
async fn read_lines(read: OwnedReadHalf, tx: mpsc::Sender<String>) {
    let mut reader = BufReader::new(read);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_matches(['\r', '\n']);
        if !line.is_empty() && tx.send(line.to_owned()).await.is_err() {
            return;
        }
    }
}

/// The connection task: sends one request at a time and demultiplexes server lines.
async fn run(
    mut write: OwnedWriteHalf,
    mut lines: Lines,
    mut requests: mpsc::Receiver<Request>,
    notifications: mpsc::Sender<Notification>,
) {
    let mut keepalive = interval_at(Instant::now() + KEEPALIVE, KEEPALIVE);
    let mut current: Option<(Option<Reply>, Vec<Record>)> = None;
    loop {
        tokio::select! {
            line = lines.rx.recv() => {
                let Some(line) = line else { return };
                if line.starts_with("notify") {
                    if notifications.try_send(parse_notification(&line)).is_err_and(|e| matches!(e, mpsc::error::TrySendError::Full(_))) {
                        tracing::warn!(target: "teamspeak::query", "notification backlog full, dropping");
                    }
                } else if let Some(rest) = line.strip_prefix("error ") {
                    if let Some((reply, records)) = current.take()
                        && let Some(reply) = reply
                    {
                        let _ = reply.send(parse_error(rest).map(|()| records));
                    }
                } else if let Some((_, records)) = current.as_mut() {
                    records.extend(parse_records(&line));
                } else {
                    tracing::debug!(target: "teamspeak::query", "unexpected line: {line}");
                }
            }
            request = requests.recv(), if current.is_none() => {
                let Some(Request { line, reply }) = request else { return };
                if write.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                    return;
                }
                current = Some((reply, Vec::new()));
            }
            _ = keepalive.tick(), if current.is_none() => {
                if write.write_all(b"whoami\n").await.is_err() {
                    return;
                }
                current = Some((None, Vec::new()));
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    #[test]
    fn escaping_roundtrip() {
        let samples = ["", "plain", "a b|c/d\\e", "tab\tnl\ncr\rbell\x07bs\x08ff\x0cvt\x0b", "ünï cödé 日本"];
        for s in samples {
            assert_eq!(unescape(&escape(s)), s, "{s:?}");
            assert!(!escape(s).contains([' ', '|', '\n', '\r']));
        }
        assert_eq!(escape("a b/c|d\\"), "a\\sb\\/c\\pd\\\\");
        assert_eq!(unescape("Hello\\sWorld\\p\\/"), "Hello World|/");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    #[test]
    fn cmd_line() {
        let cmd = Cmd::new("clientkick").arg("clid", 5).arg("reasonmsg", "bye now").flag("-uid").flag("x");
        assert_eq!(cmd.line(), "clientkick clid=5 reasonmsg=bye\\snow -uid -x");
        let multi = Cmd::new("channeladdperm")
            .arg("cid", 3)
            .arg("permsid", "a")
            .arg("permvalue", 1)
            .record()
            .arg("permsid", "b");
        assert_eq!(multi.line(), "channeladdperm cid=3 permsid=a permvalue=1|permsid=b");
        assert_eq!(Cmd::new("serverinfo").line(), "serverinfo");
    }

    #[test]
    fn parses_records_and_notifications() {
        let recs = parse_records("clid=1 client_nickname=Foo\\sBar client_away|clid=2 client_nickname=x\\pz");
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["client_nickname"], "Foo Bar");
        assert_eq!(recs[0]["client_away"], "");
        assert_eq!(recs[1]["client_nickname"], "x|z");
        let n = parse_notification("notifytextmessage targetmode=1 msg=hi\\sthere invokerid=7");
        assert_eq!(n.name, "notifytextmessage");
        assert_eq!(n.records[0]["msg"], "hi there");
        assert_eq!(parse_notification("notifyfoo").records, vec![Record::new()]);
    }

    #[test]
    fn parses_errors_and_flood() {
        assert!(parse_error("id=0 msg=ok").is_ok());
        let e = parse_error("id=1281 msg=database\\sempty\\sresult\\sset").unwrap_err();
        assert_eq!((e.id, e.msg.as_str()), (1281, "database empty result set"));
        let flood = parse_error("id=524 msg=client\\sis\\sflooding extra_msg=please\\swait\\s3\\sseconds").unwrap_err();
        assert_eq!(flood_wait(&flood), Some(Duration::from_secs(3)));
        assert_eq!(flood_wait(&e), None);
    }

    async fn read_command(sock: &mut TcpStream, buf: &mut Vec<u8>) -> String {
        loop {
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                return String::from_utf8(line).unwrap().trim().to_owned();
            }
            let mut chunk = [0u8; 512];
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Fake server speaking with `\n\r` terminators like the real one; `eol` lets a test pick another.
    async fn fake_server(eol: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut flooded = false;
            sock.write_all(format!("TS3{eol}Welcome to the TeamSpeak 3 ServerQuery interface{eol}").as_bytes())
                .await
                .unwrap();
            loop {
                let cmd = read_command(&mut sock, &mut buf).await;
                let out = match cmd.split(' ').next().unwrap() {
                    "login" | "use" | "servernotifyregister" => {
                        assert!(
                            cmd.starts_with("login client_login_name=serveradmin client_login_password=p\\ss")
                                || !cmd.starts_with("login"),
                            "{cmd}"
                        );
                        format!("error id=0 msg=ok{eol}")
                    }
                    // Notifications before, between and after the data, plus a notification that looks like data.
                    "clientlist" => format!(
                        "notifyclientmoved clid=9 ctid=2{eol}clid=1 client_nickname=A\\sB|clid=2 client_nickname=C{eol}\
                         notifytextmessage msg=x{eol}error id=0 msg=ok{eol}notifyclientleftview clid=2{eol}"
                    ),
                    "boom" => format!("error id=2568 msg=insufficient\\sclient\\spermissions failed_permid=4{eol}"),
                    "flood" if !flooded => {
                        flooded = true;
                        format!("error id=524 msg=client\\sis\\sflooding extra_msg=please\\swait\\s1\\sseconds{eol}")
                    }
                    "flood" => format!("ok=1{eol}error id=0 msg=ok{eol}"),
                    "bye" => return,
                    other => panic!("unexpected command {other}"),
                };
                sock.write_all(out.as_bytes()).await.unwrap();
            }
        });
        addr
    }

    #[derive(Debug, Clone)]
    enum ChannelInput {
        Text { cid: u64, mode: u8, invoker: u16, text: String },
        Disconnect(u64),
    }

    pub(crate) enum ChannelEvent {
        Ready { cid: u64, commands: Vec<String> },
        Closed(u64),
    }

    pub(crate) struct ChannelServer {
        pub addr: SocketAddr,
        input: tokio::sync::broadcast::Sender<ChannelInput>,
        pub events: mpsc::UnboundedReceiver<ChannelEvent>,
        task: JoinHandle<()>,
    }

    impl ChannelServer {
        pub fn message(&self, cid: u64, mode: u8, invoker: u16, text: &str) {
            self.input.send(ChannelInput::Text { cid, mode, invoker, text: text.to_owned() }).unwrap();
        }

        pub fn disconnect(&self, cid: u64) {
            self.input.send(ChannelInput::Disconnect(cid)).unwrap();
        }

        pub async fn event(&mut self) -> ChannelEvent {
            timeout(Duration::from_secs(8), self.events.recv()).await.unwrap().unwrap()
        }
    }

    impl Drop for ChannelServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// Each fake query client hears text only in its current channel after registration.
    pub(crate) async fn channel_server() -> ChannelServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (input, _) = tokio::sync::broadcast::channel(256);
        let (events, rx) = mpsc::unbounded_channel();
        let server_input = input.clone();
        let task = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            let mut clid = 100u16;
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (sock, _) = accepted.unwrap();
                        clients.spawn(channel_client(sock, clid, server_input.subscribe(), events.clone()));
                        clid += 1;
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        ChannelServer { addr, input, events: rx, task }
    }

    async fn channel_client(
        sock: TcpStream,
        clid: u16,
        mut input: tokio::sync::broadcast::Receiver<ChannelInput>,
        events: mpsc::UnboundedSender<ChannelEvent>,
    ) {
        let (read, mut write) = sock.into_split();
        let mut lines = BufReader::new(read).lines();
        let mut cid = 1;
        let mut registered = false;
        let mut commands = Vec::new();
        write.write_all(b"TS3\n\rWelcome to the TeamSpeak 3 ServerQuery interface\n\r").await.unwrap();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    let (name, args) = line.split_once(' ').unwrap_or((&line, ""));
                    let args = parse_records(args).remove(0);
                    let mut reply = String::new();
                    let mut error = 0;
                    match name {
                        "login" => {
                            assert_eq!(args["client_login_name"], "serveradmin");
                            assert_eq!(args["client_login_password"], "p ss");
                        }
                        "use" => assert_eq!(args["sid"], "1"),
                        "clientupdate" => assert!(args["client_nickname"].starts_with("Gwar chat ")),
                        "whoami" => reply = format!("client_id={clid} client_channel_id={cid}\n\r"),
                        "clientmove" => {
                            assert_eq!(args["clid"], clid.to_string());
                            let next = args["cid"].parse().unwrap();
                            if cid == next { error = 770; }
                            cid = next;
                        }
                        "servernotifyregister" => {
                            assert_eq!(args["event"], "textchannel");
                            registered = true;
                        }
                        other => panic!("unexpected command {other}"),
                    }
                    commands.push(line);
                    if registered && commands.last().unwrap().starts_with("servernotifyregister") {
                        let _ = events.send(ChannelEvent::Ready { cid, commands: commands.clone() });
                    }
                    reply.push_str(&format!("error id={error} msg=ok\n\r"));
                    if write.write_all(reply.as_bytes()).await.is_err() { break; }
                }
                message = input.recv() => match message {
                    Ok(ChannelInput::Text { cid: channel, mode, invoker, text }) if registered && channel == cid => {
                        let line = format!("notifytextmessage targetmode={mode} invokerid={invoker} msg={}\n\r", escape(&text));
                        if write.write_all(line.as_bytes()).await.is_err() { break; }
                    }
                    Ok(ChannelInput::Disconnect(channel)) if channel == cid => break,
                    Ok(_) => {},
                    Err(_) => break,
                },
            }
        }
        let _ = events.send(ChannelEvent::Closed(cid));
    }

    #[tokio::test]
    async fn channel_listener_setup_and_text() {
        let mut server = channel_server().await;
        let (query, mut notes) = Query::listen_channel(server.addr, "p ss", 42).await.unwrap();
        let ChannelEvent::Ready { cid, commands } = server.event().await else { panic!("listener not ready") };
        assert_eq!(cid, 42);
        assert_eq!(
            commands,
            [
                "login client_login_name=serveradmin client_login_password=p\\sss",
                "use sid=1",
                "clientupdate client_nickname=Gwar\\schat\\s42",
                "whoami",
                "clientmove clid=100 cid=42",
                "servernotifyregister event=textchannel",
            ]
        );
        server.message(99, 2, 7, "another channel");
        server.message(42, 2, 7, "[b]Hi[/b] | / \\\n日本");
        let note = timeout(Duration::from_secs(3), notes.recv()).await.unwrap().unwrap();
        assert_eq!(note.name, "notifytextmessage");
        assert_eq!(note.records[0]["invokerid"], "7");
        assert_eq!(note.records[0]["msg"], "[b]Hi[/b] | / \\\n日本");
        assert!(notes.try_recv().is_err());
        drop(query);
        assert!(matches!(server.event().await, ChannelEvent::Closed(42)));
        assert!(notes.recv().await.is_none());
    }

    #[tokio::test]
    async fn listener_can_start_in_default_channel_and_query_clones_keep_socket_alive() {
        let mut server = channel_server().await;
        let (query, notes) = Query::listen_channel(server.addr, "p ss", 1).await.unwrap();
        assert!(matches!(server.event().await, ChannelEvent::Ready { cid: 1, .. }));
        let clone = query.clone();
        drop(query);
        drop(notes);
        assert_eq!(clone.call(Cmd::new("whoami")).await.unwrap()[0]["client_id"], "100");
        drop(clone);
        assert!(matches!(server.event().await, ChannelEvent::Closed(1)));
    }

    async fn interleaving(eol: &'static str) {
        let addr = fake_server(eol).await;
        let (query, mut notes) = Query::connect(addr, "serveradmin", "p ss", 1).await.unwrap();
        query.call(Cmd::new("servernotifyregister").arg("event", "server")).await.unwrap();

        let list = query.call(Cmd::new("clientlist")).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["client_nickname"], "A B");
        assert_eq!(list[1]["clid"], "2");
        let names: Vec<_> = [notes.recv().await.unwrap(), notes.recv().await.unwrap(), notes.recv().await.unwrap()]
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert_eq!(names, ["notifyclientmoved", "notifytextmessage", "notifyclientleftview"]);

        let err = query.call(Cmd::new("boom")).await.unwrap_err();
        let qe = err.downcast_ref::<QueryError>().unwrap();
        assert_eq!(qe.id, 2568);
        assert_eq!(qe.msg, "insufficient client permissions");

        // Flood protection: one automatic retry after the indicated wait.
        let started = std::time::Instant::now();
        let ok = query.call(Cmd::new("flood")).await.unwrap();
        assert_eq!(ok[0]["ok"], "1");
        assert!(started.elapsed() >= Duration::from_secs(1));

        // The server hanging up closes the notification channel and fails further calls.
        let _ = query.call(Cmd::new("bye")).await.unwrap_err();
        assert!(notes.recv().await.is_none());
        assert!(query.call(Cmd::new("clientlist")).await.is_err());
    }

    #[tokio::test]
    async fn reply_notification_interleaving_nr() {
        interleaving("\n\r").await;
    }

    #[tokio::test]
    async fn reply_notification_interleaving_crlf() {
        interleaving("\r\n").await;
    }

    #[tokio::test]
    async fn rejects_foreign_banner() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(b"SSH-2.0-OpenSSH\r\nmore\r\n").await.unwrap();
            sleep(Duration::from_secs(1)).await;
        });
        assert!(Query::connect(addr, "a", "b", 1).await.is_err());
    }
}
