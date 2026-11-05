//! End-to-end tests of `rexctl` against a scripted, in-process stand-in for a Rex server's
//! control endpoint. Nothing here contacts a real server.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rex_relay::registry::{Owned, Registry};
use serde_json::{Value, json};
use tungstenite::Message;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};

const TERMINAL: &str = "com.superlogical.terminal";
const BOX: [&str; 4] = [
    "╭────────╮",
    "│ >      │",
    "╰────────╯",
    "  ? for shortcuts",
];

/// What the fake Claude Code, and the fake server's connection, do once Enter arrives. Each
/// but `StallThenDone` starts by reporting Working.
#[derive(Clone, Copy, Debug)]
enum AfterEnter {
    Done,
    Blocked(&'static str),
    KeepWorking,
    /// Works and finishes, but the event stream has stopped, so no event arrives: only a
    /// status read shows the new Done.
    StallThenDone,
    /// Drops the connection and finishes before the client is back. The next `refuse`
    /// connection attempts fail.
    DropThenDone {
        refuse: usize,
    },
    /// Drops the connection, and finishes with an event once the client has reconnected.
    DropThenDoneEvent,
    /// Stops answering on this connection, without closing it, and finishes.
    FreezeThenDone,
    /// Drops the connection and refuses every later one.
    DropForever,
    /// Clears its records for a moment, as Claude Code does mid-turn, then keeps working.
    BlipThenKeepWorking,
}

#[derive(Clone, Debug)]
struct Block {
    id: &'static str,
    foreground: &'static str,
    records: Vec<Value>,
    bracketed_paste: bool,
}

struct State {
    sessions: Vec<(String, String, Vec<Block>)>,
    screen: Vec<String>,
    after_enter: AfterEnter,
    writes: Vec<(String, String, String)>,
    created: Vec<Value>,
    /// Every method called, in order.
    methods: Vec<String>,
    /// Every hello, one per connection that got as far as the protocol.
    hellos: Vec<Value>,
    /// How many upcoming connections to drop before the WebSocket handshake.
    refuse: usize,
    /// A record to report as an event once a reconnected client reads the status.
    after_reconnect: Option<(String, String, Value)>,
    /// Unsent text in the prompt box (the line equal to `BOX[1]`).
    input: String,
    /// The input box ignores Ctrl+U.
    sticky_input: bool,
    /// How many more process reports show a shell pane running a command (`usize::MAX`:
    /// forever).
    shell_busy: usize,
    /// What `shell_busy` becomes when Enter reaches a shell pane.
    busy_after_enter: usize,
}

fn screen(body: &[&str]) -> Vec<String> {
    body.iter()
        .chain(&[""])
        .chain(&BOX)
        .map(|s| s.to_string())
        .collect()
}

/// A record with a fresh `updated_at`, as a program makes for each new report.
fn fresh(state: &str, kind: Option<&str>, msg: Option<&str>) -> Value {
    static REPORTS: AtomicU64 = AtomicU64::new(1);
    let mut r = record(state, kind, msg);
    let n = REPORTS.fetch_add(1, Ordering::SeqCst);
    r["updated_at"] = json!(format!("2026-11-04T01:00:00.{n:06}Z"));
    r
}

fn record(state: &str, kind: Option<&str>, msg: Option<&str>) -> Value {
    let mut r = json!({"state": state, "app": "claude", "effective_app": "claude", "updated_at": "2026-11-04T00:00:00Z"});
    if let Some(kind) = kind {
        r["kind"] = json!(kind);
    }
    if let Some(msg) = msg {
        r["msg"] = json!(msg);
    }
    r
}

fn claude(records: Vec<Value>) -> Block {
    Block {
        id: "block:c1a0de00-0000-0000-0000-000000000002",
        foreground: "claude",
        records,
        bracketed_paste: true,
    }
}

fn shell() -> Block {
    Block {
        id: "block:5he11000-0000-0000-0000-000000000001",
        foreground: "bash",
        records: vec![],
        bracketed_paste: false,
    }
}

struct Mock {
    port: u16,
    state: Arc<Mutex<State>>,
    connections: Arc<AtomicUsize>,
    dir: tempfile::TempDir,
}

impl Mock {
    fn start(sessions: Vec<(&str, &str, Vec<Block>)>, after_enter: AfterEnter) -> Mock {
        let state = Arc::new(Mutex::new(State {
            sessions: sessions
                .into_iter()
                .map(|(id, label, blocks)| (id.to_owned(), label.to_owned(), blocks))
                .collect(),
            screen: screen(&["Welcome to Claude Code"]),
            after_enter,
            writes: vec![],
            created: vec![],
            methods: vec![],
            hellos: vec![],
            refuse: 0,
            after_reconnect: None,
            input: String::new(),
            sticky_input: false,
            shell_busy: 0,
            busy_after_enter: 0,
        }));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        let (shared, count) = (state.clone(), connections.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                count.fetch_add(1, Ordering::SeqCst);
                let shared = shared.clone();
                std::thread::spawn(move || serve(stream, shared));
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let mock = Mock {
            port,
            state,
            connections,
            dir,
        };
        mock.write_config(&[&mock.endpoint()]);
        mock
    }

    fn endpoint(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    fn config_path(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }

    fn audit_path(&self) -> PathBuf {
        self.dir.path().join("state/audit.jsonl")
    }

    fn write_config(&self, allowed: &[&str]) {
        self.write_config_with(allowed, "");
    }

    fn write_config_with(&self, allowed: &[&str], extra: &str) {
        let allowed: Vec<String> = allowed.iter().map(|a| format!("{a:?}")).collect();
        std::fs::write(
            self.config_path(),
            format!(
                "allowed_endpoints = [{}]\naudit_log = {:?}\n{extra}",
                allowed.join(", "),
                self.audit_path().display().to_string()
            ),
        )
        .unwrap();
    }

    fn hellos(&self) -> Vec<Value> {
        self.state.lock().unwrap().hellos.clone()
    }

    fn rexctl(&self, args: &[&str]) -> Output {
        let endpoint = self.endpoint();
        rexctl(self.dir.path(), &self.config_path(), &endpoint, args)
    }

    fn writes(&self) -> Vec<(String, String, String)> {
        self.state.lock().unwrap().writes.clone()
    }

    fn methods(&self) -> Vec<String> {
        self.state.lock().unwrap().methods.clone()
    }

    fn rename(&self, session_id: &str, label: &str) {
        let mut state = self.state.lock().unwrap();
        let session = state
            .sessions
            .iter_mut()
            .find(|s| s.0 == session_id)
            .unwrap();
        session.1 = label.to_owned();
    }

    /// The endpoint as rexctl records it in the registry.
    fn canonical_endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn registry(&self) -> Registry {
        Registry::new(self.dir.path().join("xdg-state/rex-relay/owned.json"))
    }

    /// Records a session as created by rexctl on this mock's endpoint.
    fn own(&self, session_id: &str, purpose: Option<&str>) {
        self.registry()
            .add(Owned {
                endpoint: self.canonical_endpoint(),
                session_id: session_id.into(),
                label: Some("original".into()),
                created_at: "2026-11-04T00:00:00.000Z".into(),
                purpose: purpose.map(Into::into),
            })
            .unwrap();
    }

    fn owned_ids(&self) -> Vec<String> {
        self.registry()
            .load()
            .unwrap()
            .into_iter()
            .map(|o| o.session_id.to_string())
            .collect()
    }

    fn audit(&self) -> Vec<Value> {
        match std::fs::read_to_string(self.audit_path()) {
            Ok(text) => text
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect(),
            Err(_) => vec![],
        }
    }
}

fn rexctl(home: &Path, config: &Path, endpoint: &str, args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_rexctl"))
        .arg("--config")
        .arg(config)
        .arg("--endpoint")
        .arg(endpoint)
        .args(args)
        .env_remove("REX_SERVER")
        .env_remove("REX_RELAY_CONFIG")
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("xdg-state"))
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn json_lines(output: &Output) -> Vec<Value> {
    stdout(output)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?}: {e}")))
        .collect()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

// ---- The fake server ----

struct Conn {
    socket: tungstenite::WebSocket<std::net::TcpStream>,
    seq: u64,
}

impl Conn {
    fn send(&mut self, value: Value) {
        self.socket.send(Message::text(value.to_string())).unwrap();
    }

    /// Reports a new status record: stores it, then sends the event.
    fn report(&mut self, state: &Mutex<State>, session: &str, block: &str, record: Value) {
        set_records(state, session, block, record.clone());
        self.block_event(
            session,
            block,
            "program_status_changed",
            json!({"id": "", "record": record, "reason": "report"}),
        );
    }

    fn block_event(&mut self, session: &str, block: &str, name: &str, payload: Value) {
        self.seq += 1;
        let seq = self.seq;
        self.send(json!({"type": "event", "seq": seq, "payload": {
            "type": "session_event",
            "payload": {"session_id": session, "event": {"type": "block_event", "payload": {
                "block_id": block, "creator_name": TERMINAL, "name": name, "payload": payload,
            }}}
        }}));
    }
}

// tungstenite fixes the callback's error type.
#[allow(clippy::result_large_err)]
fn accept_control(_: &Request, mut response: Response) -> Result<Response, ErrorResponse> {
    response
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", "rex.control.v1".parse().unwrap());
    Ok(response)
}

/// The screen as `format` returns it: `vt` keeps escape sequences, `text` drops them.
fn render(state: &State, vt: bool) -> String {
    let lines: Vec<String> = state
        .screen
        .iter()
        .map(|line| {
            if line == BOX[1] && !state.input.is_empty() {
                format!("│ > {} │", state.input)
            } else {
                line.clone()
            }
        })
        .collect();
    if vt {
        return lines.join("\r\n");
    }
    let mut text = lines.join("\n");
    while let Some(start) = text.find('\x1b') {
        let end = text[start..]
            .find('m')
            .map_or(text.len(), |e| start + e + 1);
        text.replace_range(start..end, "");
    }
    text
}

fn clear_records(state: &Mutex<State>, session: &str, block: &str) {
    let mut state = state.lock().unwrap();
    let blocks = &mut state
        .sessions
        .iter_mut()
        .find(|s| s.0 == session)
        .unwrap()
        .2;
    blocks
        .iter_mut()
        .find(|b| b.id == block)
        .unwrap()
        .records
        .clear();
}

/// Stores `record` as the pane's only record, as a status read would return it.
fn set_records(state: &Mutex<State>, session: &str, block: &str, record: Value) {
    let mut state = state.lock().unwrap();
    let blocks = &mut state
        .sessions
        .iter_mut()
        .find(|s| s.0 == session)
        .unwrap()
        .2;
    blocks.iter_mut().find(|b| b.id == block).unwrap().records = vec![record];
}

fn serve(stream: std::net::TcpStream, state: Arc<Mutex<State>>) {
    {
        let mut s = state.lock().unwrap();
        if s.refuse > 0 {
            s.refuse -= 1;
            return;
        }
    }
    let Ok(socket) = tungstenite::accept_hdr(stream, accept_control) else {
        return;
    };
    let mut conn = Conn { socket, seq: 0 };
    let Ok(Message::Text(hello)) = conn.socket.read() else {
        return;
    };
    let hello: Value = serde_json::from_str(hello.as_str()).unwrap();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["api_revision"], 8);
    let reconnected = {
        let mut s = state.lock().unwrap();
        s.hellos.push(hello);
        s.hellos.len() > 1
    };
    conn.send(json!({
        "type": "welcome", "protocol": "rex.control.v1",
        "limits": {"max_message_bytes": 1048576},
        "client": {"id": "client:1", "reconnect_token": "token-1", "resumed": reconnected},
        "api_revision": 8, "api_server_revision": 8, "api_min_revision": 7,
    }));
    loop {
        let text = match conn.socket.read() {
            Ok(Message::Text(text)) => text.as_str().to_owned(),
            Ok(Message::Close(_)) | Err(_) => return,
            Ok(_) => continue,
        };
        let request: Value = serde_json::from_str(&text).unwrap();
        let id = request["id"].clone();
        let method = request["method"].as_str().unwrap().to_owned();
        let payload = &request["payload"];
        let (reply, then) = handle(&state, &method, payload, reconnected);
        let seq = conn.seq;
        match reply {
            Ok(value) => {
                conn.send(json!({"type": "response", "id": id, "seq": seq, "payload": value}))
            }
            Err(message) => conn.send(json!({"type": "response", "id": id, "seq": seq,
                "error": {"code": "not_found", "message": message, "status": 404}})),
        }
        match then {
            Then::Nothing => {}
            Then::Report(session, block, r) => conn.report(&state, &session, &block, r),
            Then::Enter(session, block, after) => {
                if !matches!(after, AfterEnter::StallThenDone) {
                    conn.report(&state, &session, &block, fresh("working", None, None));
                }
                match after {
                    AfterEnter::Done => {
                        conn.report(&state, &session, &block, fresh("done", None, None));
                    }
                    AfterEnter::Blocked(kind) => {
                        let r = fresh("blocked", Some(kind), Some("Allow Bash(cargo test)?"));
                        conn.report(&state, &session, &block, r);
                    }
                    AfterEnter::KeepWorking => {}
                    AfterEnter::StallThenDone => {
                        set_records(&state, &session, &block, fresh("done", None, None));
                    }
                    AfterEnter::DropThenDone { refuse } => {
                        set_records(&state, &session, &block, fresh("done", None, None));
                        state.lock().unwrap().refuse = refuse;
                        return;
                    }
                    AfterEnter::DropThenDoneEvent => {
                        state.lock().unwrap().after_reconnect =
                            Some((session, block, fresh("done", None, None)));
                        return;
                    }
                    AfterEnter::FreezeThenDone => {
                        set_records(&state, &session, &block, fresh("done", None, None));
                        // Hold the socket open and answer nothing.
                        std::thread::sleep(std::time::Duration::from_secs(60));
                        return;
                    }
                    AfterEnter::DropForever => {
                        state.lock().unwrap().refuse = usize::MAX;
                        return;
                    }
                    AfterEnter::BlipThenKeepWorking => {
                        clear_records(&state, &session, &block);
                        conn.block_event(
                            &session,
                            &block,
                            "program_status_removed",
                            json!({"ids": [""], "reason": "report"}),
                        );
                        conn.report(&state, &session, &block, fresh("working", None, None));
                    }
                }
            }
        }
    }
}

/// What the server does after it replies.
enum Then {
    Nothing,
    Enter(String, String, AfterEnter),
    Report(String, String, Value),
}

fn handle(
    state: &Mutex<State>,
    method: &str,
    payload: &Value,
    reconnected: bool,
) -> (Result<Value, String>, Then) {
    let mut s = state.lock().unwrap();
    s.methods.push(method.to_owned());
    let sessions: Vec<Value> = s
        .sessions
        .iter()
        .map(|(id, label, _)| json!({"session_id": id, "label": label}))
        .collect();
    let sid = payload["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let bid = payload["block_id"].as_str().unwrap_or_default().to_owned();
    let session = s.sessions.iter().find(|(id, ..)| *id == sid).cloned();
    let block = session
        .as_ref()
        .and_then(|(_, _, blocks)| blocks.iter().find(|b| b.id == bid).cloned());
    let view = |session: &(String, String, Vec<Block>)| {
        let blocks: Vec<Value> = session
            .2
            .iter()
            .map(|b| json!({"block_id": b.id, "creator_name": TERMINAL, "flavor": "com.superlogical.terminal.shell",
                "label": if b.foreground == "claude" { "claude" } else { "shell" }}))
            .collect();
        json!({
            "revision": 1, "session_id": session.0, "label": session.1,
            "active_window_id": "window:1",
            "windows": [{"window_id": "window:1", "label": "main", "focused_block_id": session.2.last().map(|b| b.id),
                "layers": [{"kind": "tiled", "blocks": blocks}]}],
        })
    };
    let reply = match method {
        "session.list" | "session.attach_all" => Ok(json!({"sessions": sessions})),
        "session.attach" => match &session {
            Some(session) => Ok(json!({"client_id": "client:1", "view": view(session)})),
            None => Err("no such session".into()),
        },
        "session.view" => session
            .as_ref()
            .map(view)
            .ok_or_else(|| "no such session".into()),
        "session.create" => {
            s.created.push(payload.clone());
            // Rex names a session itself when the request has no label.
            let label = payload["label"].as_str().unwrap_or("Session 2").to_owned();
            s.sessions
                .push(("session:new".into(), label, vec![claude(vec![])]));
            Ok(json!({"session_id": "session:new", "initial_windows": [
                {"window_id": "window:new", "block_ids": ["block:new"], "revision": 1}]}))
        }
        "session.destroy" => {
            let before = s.sessions.len();
            s.sessions.retain(|(id, ..)| *id != sid);
            if s.sessions.len() == before {
                Err("no such session".into())
            } else {
                Ok(json!({}))
            }
        }
        m if m.starts_with(TERMINAL) => {
            let Some(block) = block else {
                return (Err("no such block".into()), Then::Nothing);
            };
            match &m[TERMINAL.len()..] {
                ".program_status" => {
                    let reply = Ok(json!({"records": block.records}));
                    if reconnected && let Some((sid, bid, r)) = s.after_reconnect.take() {
                        return (reply, Then::Report(sid, bid, r));
                    }
                    reply
                }
                ".process" => {
                    let mut name = block.foreground;
                    if name != "claude" && s.shell_busy > 0 {
                        if s.shell_busy != usize::MAX {
                            s.shell_busy -= 1;
                        }
                        name = "make";
                    }
                    Ok(json!({"foreground": {"pid": 7, "user_id": 1000, "name": name}}))
                }
                ".title" => Ok(json!({"title": ""})),
                ".modes" => Ok(json!({"screen": "primary", "modes": [
                    {"name": "bracketed_paste", "number": 2004, "set": block.bracketed_paste}]})),
                ".format" => {
                    let vt = payload["args"]["format"] == "vt";
                    Ok(json!({"content": render(&s, vt)}))
                }
                ".write" => {
                    let data = payload["args"]["data"].as_str().unwrap().to_owned();
                    s.writes.push((sid.clone(), bid.clone(), data.clone()));
                    let reply = Ok(json!({"bytes_written": data.len()}));
                    if block.foreground != "claude" {
                        // A shell: typed text joins the prompt line; Enter runs it.
                        if data == "\r" {
                            let line = s.screen.last().cloned().unwrap_or_default();
                            let command = line.strip_prefix("$ ").unwrap_or("").to_owned();
                            s.screen.push(format!("output of {command}"));
                            s.screen.push("$ ".into());
                            s.shell_busy = s.busy_after_enter;
                        } else if let Some(last) = s.screen.last_mut() {
                            last.push_str(&data);
                        }
                        return (reply, Then::Nothing);
                    }
                    if data == "\x15" && !s.sticky_input {
                        s.input.clear();
                    }
                    if data == "\r" {
                        s.screen = screen(&[
                            "Welcome to Claude Code",
                            "> fix the bug",
                            "● Fixed it.",
                            "  Updated src/lib.rs",
                        ]);
                        return (reply, Then::Enter(sid, bid, s.after_enter));
                    }
                    reply
                }
                _ => Err(format!("unknown method {m}")),
            }
        }
        _ => Err(format!("unknown method {method}")),
    };
    (reply, Then::Nothing)
}

// ---- Tests ----

const SID: &str = "session:0000-demo";

const FOREIGN: &str = "session:0000-work";

/// An owned session `demo` and a foreign one, `work`.
fn demo(records: Vec<Value>, after: AfterEnter) -> Mock {
    let mock = Mock::start(
        vec![
            (SID, "demo", vec![shell(), claude(records)]),
            (FOREIGN, "work", vec![claude(vec![])]),
        ],
        after,
    );
    mock.own(SID, Some("demo the relay"));
    mock
}

#[test]
fn send_pastes_streams_and_exits_done() {
    let mock = demo(vec![record("done", None, None)], AfterEnter::Done);
    let output = mock.rexctl(&["send", "demo", "fix the bug\nplease", "--poll", "100ms"]);
    assert_eq!(code(&output), 0, "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("● Fixed it."), "{text}");
    assert!(
        !text.contains("Welcome to Claude Code"),
        "baseline repeated: {text}"
    );

    let claude_block = claude(vec![]).id;
    assert_eq!(
        mock.writes(),
        [
            (
                SID.into(),
                claude_block.into(),
                "\x1b[200~fix the bug\nplease\x1b[201~".into()
            ),
            (SID.into(), claude_block.into(), "\r".into()),
        ]
    );

    let audit = mock.audit();
    assert_eq!(audit.len(), 2, "{audit:?}");
    assert_eq!(audit[0]["result"], "sent");
    assert_eq!(audit[1]["result"], "done");
    for entry in &audit {
        assert_eq!(entry["command"], "send");
        assert_eq!(entry["session"], SID);
        assert_eq!(entry["block"], claude_block);
        assert_eq!(entry["msg_len"], 18);
        assert_eq!(entry["endpoint"], format!("http://127.0.0.1:{}", mock.port));
        assert_eq!(
            entry["msg_sha256"],
            rex_relay::audit::sha256_hex(b"fix the bug\nplease")
        );
    }
    let raw = std::fs::read_to_string(mock.audit_path()).unwrap();
    assert!(!raw.contains("fix the bug"));
}

/// `send` with JSON output, waiting at most `timeout`.
fn send_json(mock: &Mock, timeout: &str) -> (Output, Vec<Value>) {
    let output = mock.rexctl(&[
        "--json",
        "send",
        "demo",
        "go",
        "--quiet",
        "--timeout",
        timeout,
    ]);
    let events = json_lines(&output);
    (output, events)
}

fn connection_states(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter(|e| e["type"] == "connection")
        .map(|e| e["state"].as_str().unwrap())
        .collect()
}

#[test]
fn send_catches_done_by_polling_when_events_stop() {
    // The pane was already Done before the message; only the new report's time tells them
    // apart.
    let mock = demo(vec![record("done", None, None)], AfterEnter::StallThenDone);
    let (output, events) = send_json(&mock, "20s");
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(events.last().unwrap()["status"], "done");
    assert!(connection_states(&events).is_empty(), "{events:?}");
    assert_eq!(mock.writes().len(), 2);
}

#[test]
fn a_brief_idle_mid_turn_is_not_done() {
    // In live use, Claude Code cleared its status record for a moment during its first turn,
    // and send took the Idle for Done while Claude Code was still thinking.
    let mock = demo(vec![], AfterEnter::BlipThenKeepWorking);
    let (output, events) = send_json(&mock, "2s");
    assert_eq!(code(&output), 15, "still working: {output:?}");
    assert_eq!(events.last().unwrap()["last_status"], "working");
}

#[test]
fn send_reconnects_after_a_drop_and_follows_events_on_the_new_connection() {
    let mock = demo(vec![], AfterEnter::DropThenDoneEvent);
    let (output, events) = send_json(&mock, "20s");
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(connection_states(&events), ["lost", "restored"]);
    assert_eq!(events.last().unwrap()["status"], "done");
    // The new connection continues the old client and joins the session again.
    let hellos = mock.hellos();
    assert_eq!(hellos.len(), 2);
    assert!(hellos[0].get("reconnect_token").is_none());
    assert_eq!(hellos[1]["reconnect_token"], "token-1");
    let attaches = mock
        .methods()
        .iter()
        .filter(|m| *m == "session.attach")
        .count();
    assert_eq!(attaches, 2);
    // The message went out once, before the drop, and was never typed again.
    assert_eq!(mock.writes().len(), 2);
    assert_eq!(mock.audit().last().unwrap()["result"], "done");
}

#[test]
fn send_catches_a_done_reported_while_disconnected() {
    // Claude Code finishes during an outage that outlasts the first reconnect attempt.
    let mock = demo(vec![], AfterEnter::DropThenDone { refuse: 1 });
    let (output, events) = send_json(&mock, "20s");
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(connection_states(&events), ["lost", "retrying", "restored"]);
    assert_eq!(events.last().unwrap()["status"], "done");
    assert_eq!(mock.writes().len(), 2);
}

#[test]
fn send_reconnects_when_the_server_stops_answering() {
    // The connection stays open but goes silent, as when the event stream stopped in live
    // use. The status poll's call times out, and a new connection finds Done.
    let mock = demo(vec![], AfterEnter::FreezeThenDone);
    mock.write_config_with(&[&mock.endpoint()], "call_timeout = \"1s\"\n");
    let (output, events) = send_json(&mock, "20s");
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(connection_states(&events), ["lost", "restored"]);
    assert_eq!(events.last().unwrap()["status"], "done");
    assert_eq!(mock.writes().len(), 2);
}

#[test]
fn send_exits_16_when_the_server_stays_unreachable() {
    let mock = demo(vec![], AfterEnter::DropForever);
    let (output, events) = send_json(&mock, "3s");
    assert_eq!(code(&output), 16, "{output:?}");
    let last = events.last().unwrap();
    assert_eq!(last["status"], "lost_connection");
    assert_eq!(last["last_status"], "working");
    assert_eq!(last["exit_code"], 16);
    assert_eq!(connection_states(&events), ["lost", "retrying"]);
    assert_eq!(mock.writes().len(), 2);
    assert_eq!(mock.audit().last().unwrap()["result"], "lost_connection");
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_first_connection_is_retried_through_a_blip() {
    let mock = demo(vec![record("done", None, None)], AfterEnter::Done);
    mock.state.lock().unwrap().refuse = 2;
    let output = mock.rexctl(&["status", "demo"]);
    assert_eq!(code(&output), 0, "{output:?}");
    let err = stderr(&output);
    assert!(err.contains("connection attempt 1 to"), "{err}");
    assert!(err.contains("retrying in 1s"), "{err}");
    assert!(err.contains("connection attempt 2 to"), "{err}");
    assert!(err.contains("retrying in 2s"), "{err}");
    assert!(!err.contains("connection attempt 3"), "{err}");
    assert_eq!(mock.hellos().len(), 1);
}

#[test]
fn an_unreachable_server_exits_16_after_connect_timeout() {
    let mock = demo(vec![], AfterEnter::Done);
    mock.write_config_with(&[&mock.endpoint()], "connect_timeout = \"2s\"\n");
    mock.state.lock().unwrap().refuse = usize::MAX;
    for args in [
        &["--json", "status", "demo"][..],
        &["--json", "send", "demo", "hi"],
    ] {
        let started = std::time::Instant::now();
        let output = mock.rexctl(args);
        assert_eq!(code(&output), 16, "{args:?}: {output:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let error = &json_lines(&output)[0];
        assert_eq!(error["type"], "error");
        assert_eq!(error["exit_code"], 16);
        let message = error["message"].as_str().unwrap();
        assert!(message.contains("couldn't connect to"), "{message}");
        // One retry fits in 2s; the next pause wouldn't.
        assert!(
            stderr(&output).contains("connection attempt 1 to"),
            "{output:?}"
        );
    }
    assert!(mock.writes().is_empty());
    assert!(mock.hellos().is_empty());
    let audit = mock.audit();
    assert_eq!(audit.len(), 1, "the failed send is audited");
    assert!(
        audit[0]["result"]
            .as_str()
            .unwrap()
            .starts_with("error: couldn't connect")
    );
}

#[test]
fn quiet_prints_only_the_tail_and_the_final_status() {
    let mock = demo(vec![], AfterEnter::Done);
    let output = mock.rexctl(&["send", "demo", "fix the bug", "--quiet", "--lines", "6"]);
    assert_eq!(code(&output), 0, "{output:?}");
    // The final screen's last 6 lines, and nothing streamed before them.
    let mut tail = vec!["  Updated src/lib.rs", ""];
    tail.extend(&BOX);
    assert_eq!(stdout(&output), format!("{}\n", tail.join("\n")));
    assert_eq!(stderr(&output), "rexctl: Done\n");

    let output = mock.rexctl(&["wait", "demo", "--quiet", "--lines", "1"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(stdout(&output), format!("{}\n", BOX[3]));

    let output = mock.rexctl(&["--json", "send", "demo", "go", "--quiet", "--lines", "2"]);
    let events = json_lines(&output);
    assert!(events.iter().all(|e| e["type"] != "output"), "{events:?}");
    let tail = events.iter().find(|e| e["type"] == "tail").unwrap();
    assert_eq!(tail["lines"], json!([BOX[2], BOX[3]]));
    assert_eq!(events.last().unwrap()["type"], "result");
}

#[test]
fn send_json_reports_needs_approval() {
    let mock = demo(vec![], AfterEnter::Blocked("permission"));
    let output = mock.rexctl(&["--json", "send", "demo", "run the tests", "--poll", "100ms"]);
    assert_eq!(code(&output), 10, "{output:?}");
    let events = json_lines(&output);
    assert_eq!(events[0]["type"], "sent");
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "status" && e["status"] == "working")
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "output" && e["line"] == "● Fixed it.")
    );
    let last = events.last().unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["status"], "needs_approval");
    assert_eq!(last["label"], "Needs approval");
    assert_eq!(last["msg"], "Allow Bash(cargo test)?");
    assert_eq!(last["exit_code"], 10);
    assert_eq!(mock.audit().last().unwrap()["result"], "needs_approval");
}

#[test]
fn send_exit_codes_for_question_and_sign_in() {
    for (kind, exit) in [("question", 11), ("auth", 13)] {
        let mock = demo(vec![], AfterEnter::Blocked(kind));
        let output = mock.rexctl(&["send", "demo", "go", "--quiet"]);
        assert_eq!(code(&output), exit, "{kind}: {output:?}");
    }
}

#[test]
fn send_times_out() {
    let mock = demo(vec![], AfterEnter::KeepWorking);
    let output = mock.rexctl(&[
        "--json",
        "send",
        "demo",
        "go",
        "--timeout",
        "1s",
        "--poll",
        "200ms",
    ]);
    assert_eq!(code(&output), 15, "{output:?}");
    let last = json_lines(&output).pop().unwrap();
    assert_eq!(last["status"], "timeout");
    assert_eq!(last["last_status"], "working");
    assert_eq!(mock.audit().last().unwrap()["result"], "timeout");
}

#[test]
fn send_refuses_foreign_sessions() {
    let mock = demo(vec![], AfterEnter::Done);
    let output = mock.rexctl(&["send", "work", "hello"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--allow-foreign"));
    assert!(mock.writes().is_empty());
    let audit = mock.audit();
    assert_eq!(audit.len(), 1);
    assert!(audit[0]["result"].as_str().unwrap().starts_with("refused:"));

    let output = mock.rexctl(&["send", "work", "hello", "--allow-foreign", "--quiet"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(mock.writes().len(), 2);
}

#[test]
fn ownership_follows_the_session_id_not_the_label() {
    let mock = demo(vec![], AfterEnter::Done);
    // Claude Code renamed the owned session, and someone made a foreign session that looks
    // like one of ours.
    mock.rename(SID, "Fix the flaky CI job");
    mock.rename(FOREIGN, "demo");

    let output = mock.rexctl(&["send", "demo", "hello"]);
    assert_eq!(code(&output), 3, "lookalike label: {output:?}");
    assert!(mock.writes().is_empty());
    assert!(
        !mock.methods().contains(&"session.attach".to_owned()),
        "refused before attaching"
    );

    let output = mock.rexctl(&["send", "Fix the flaky CI job", "hello", "--quiet"]);
    assert_eq!(code(&output), 0, "current label resolves: {output:?}");
    let output = mock.rexctl(&["send", SID, "hello again", "--quiet"]);
    assert_eq!(code(&output), 0, "id resolves: {output:?}");
    assert!(mock.writes().iter().all(|(sid, ..)| sid == SID));

    // Ownership is per endpoint: the same id registered elsewhere doesn't count.
    let other = demo(vec![], AfterEnter::Done);
    other
        .registry()
        .remove(&other.canonical_endpoint(), &SID.into())
        .unwrap();
    other
        .registry()
        .add(Owned {
            endpoint: "https://rex-host.example.ts.net".into(),
            session_id: SID.into(),
            label: Some("demo".into()),
            created_at: "2026-11-04T00:00:00.000Z".into(),
            purpose: None,
        })
        .unwrap();
    let output = other.rexctl(&["send", SID, "hello"]);
    assert_eq!(code(&output), 3, "{output:?}");
}

#[test]
fn ambiguous_labels_are_an_error() {
    let mock = demo(vec![], AfterEnter::Done);
    mock.rename(FOREIGN, "demo");
    for args in [
        &["status", "demo"][..],
        &["close", "demo"],
        &["send", "demo", "hi", "--allow-foreign"],
    ] {
        let output = mock.rexctl(args);
        assert_eq!(code(&output), 1, "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("more than one session"),
            "{output:?}"
        );
    }
    assert!(mock.writes().is_empty());
    assert!(!mock.methods().contains(&"session.destroy".to_owned()));
}

#[test]
fn close_closes_owned_sessions_only() {
    let mock = demo(vec![], AfterEnter::Done);

    // Foreign sessions are never closed, and the refusal sends no close request.
    for args in [&["close", "work"][..], &["close", FOREIGN]] {
        let output = mock.rexctl(args);
        assert_eq!(code(&output), 3, "{args:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("never closes"));
    }
    let output = mock.rexctl(&["close", "work", "--allow-foreign"]);
    assert_eq!(code(&output), 2, "close has no override: {output:?}");
    assert!(!mock.methods().contains(&"session.destroy".to_owned()));
    assert!(mock.state.lock().unwrap().sessions.len() == 2);

    // An owned session, under its current label.
    mock.rename(SID, "Renamed by Claude");
    let output = mock.rexctl(&["--json", "close", "Renamed by Claude"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(json_lines(&output)[0]["session_id"], SID);
    assert_eq!(
        mock.methods()
            .iter()
            .filter(|m| *m == "session.destroy")
            .count(),
        1
    );
    let remaining: Vec<_> = mock
        .state
        .lock()
        .unwrap()
        .sessions
        .iter()
        .map(|s| s.0.clone())
        .collect();
    assert_eq!(remaining, [FOREIGN]);
    assert!(mock.owned_ids().is_empty(), "removed from the registry");

    let audit = mock.audit();
    let results: Vec<_> = audit
        .iter()
        .map(|e| e["result"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(audit.iter().filter(|e| e["command"] == "close").count(), 3);
    assert!(results[0].starts_with("refused:"));
    assert!(results[1].starts_with("refused:"));
    assert_eq!(results[2], "closed");
    assert_eq!(audit[2]["session"], SID);
    assert_eq!(audit[2]["session_label"], "Renamed by Claude");

    // Gone from both the server and the registry: there is nothing left to close.
    let output = mock.rexctl(&["close", SID]);
    assert_eq!(code(&output), 1, "{output:?}");
}

#[test]
fn close_forgets_owned_sessions_that_are_already_gone() {
    let mock = demo(vec![], AfterEnter::Done);
    mock.own("session:0000-vanished", None);
    let output = mock.rexctl(&["close", "session:0000-vanished"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(mock.owned_ids(), [SID]);
    assert!(!mock.methods().contains(&"session.destroy".to_owned()));
    assert_eq!(
        mock.audit().last().unwrap()["result"],
        "already gone; removed from registry"
    );
}

#[test]
fn a_corrupt_registry_fails_closed() {
    let mock = demo(vec![], AfterEnter::Done);
    std::fs::write(mock.registry().path(), "{oops").unwrap();
    for args in [&["send", "demo", "hi"][..], &["close", "demo"], &["list"]] {
        let output = mock.rexctl(args);
        assert_eq!(code(&output), 1, "{args:?}: {output:?}");
    }
    assert!(mock.writes().is_empty());
    assert!(!mock.methods().contains(&"session.destroy".to_owned()));
}

#[test]
fn send_refuses_pending_prompts_without_answer() {
    let mock = demo(
        vec![record("blocked", Some("question"), Some("Which file?"))],
        AfterEnter::Done,
    );
    let output = mock.rexctl(&["send", "demo", "src/lib.rs"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(mock.writes().is_empty());
    let output = mock.rexctl(&["send", "demo", "src/lib.rs", "--answer", "--quiet"]);
    assert_eq!(code(&output), 0, "{output:?}");

    let mock = demo(
        vec![record("blocked", Some("auth"), None)],
        AfterEnter::Done,
    );
    let output = mock.rexctl(&["send", "demo", "hunter2", "--answer"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(mock.writes().is_empty());
}

#[test]
fn send_refuses_menus_like_the_trust_dialog() {
    // Claude Code asks whether to trust the folder before its prompt appears. The program has
    // bracketed paste on and reports no status, so only the screen shows that Enter here would
    // pick an option.
    let mock = demo(vec![], AfterEnter::Done);
    mock.state.lock().unwrap().screen = [
        "╭──────────────────────────────────────────╮",
        "│ Do you trust the files in this folder?   │",
        "│                                          │",
        "│ /srv/app                                 │",
        "│                                          │",
        "│ ❯ 1. Yes, proceed                        │",
        "│   2. No, exit                            │",
        "╰──────────────────────────────────────────╯",
        "   Enter to confirm · Esc to exit",
    ]
    .map(String::from)
    .to_vec();
    for args in [
        &["send", "demo", "fix the bug"][..],
        &["send", "demo", "1", "--answer"],
    ] {
        let output = mock.rexctl(args);
        assert_eq!(code(&output), 3, "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("never picks menu options"),
            "{output:?}"
        );
    }
    assert!(mock.writes().is_empty());
    let audit = mock.audit();
    assert_eq!(audit.len(), 2);
    assert!(
        audit
            .iter()
            .all(|e| e["result"].as_str().unwrap().starts_with("refused:"))
    );
    assert!(audit.iter().all(|e| e["block"] == claude(vec![]).id));

    // A shell prompt isn't Claude Code's either.
    mock.state.lock().unwrap().screen = vec!["$ ls".into(), "src".into(), "$".into()];
    let output = mock.rexctl(&["send", "demo", "hello"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(mock.writes().is_empty());
}

#[test]
fn send_refuses_leftover_input_unless_told_to_clear_it() {
    let mock = demo(vec![], AfterEnter::Done);
    mock.state.lock().unwrap().input = "half a thought".into();
    let output = mock.rexctl(&["send", "demo", "fix the bug", "--quiet"]);
    assert_eq!(code(&output), 3, "{output:?}");
    let err = stderr(&output);
    assert!(err.contains("14 characters of unsent text"), "{err}");
    assert!(err.contains("--clear-input"), "{err}");
    assert!(mock.writes().is_empty());
    // The refusal is audited without the leftover text.
    let raw = std::fs::read_to_string(mock.audit_path()).unwrap();
    assert!(!raw.contains("half a thought"), "{raw}");

    let output = mock.rexctl(&["send", "demo", "fix the bug", "--quiet", "--clear-input"]);
    assert_eq!(code(&output), 0, "{output:?}");
    let keys: Vec<String> = mock.writes().into_iter().map(|w| w.2).collect();
    assert_eq!(
        keys,
        ["\x05", "\x15", "\x1b[200~fix the bug\x1b[201~", "\r"],
        "Ctrl+E and Ctrl+U, then the message"
    );
    let audit = mock.audit();
    assert_eq!(audit.last().unwrap()["input_cleared"], true);
    assert!(audit[0].get("input_cleared").is_none(), "{audit:?}");
}

#[test]
fn the_placeholder_isnt_leftover_input() {
    let mock = demo(vec![], AfterEnter::Done);
    {
        let mut state = mock.state.lock().unwrap();
        let at = state.screen.iter().position(|l| l == BOX[1]).unwrap();
        state.screen[at] = "│ > \x1b[2mTry \"fix lint errors\"\x1b[0m │".into();
    }
    let output = mock.rexctl(&["send", "demo", "hello", "--quiet"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(mock.writes().len(), 2, "no clearing keys");
}

#[test]
fn clear_input_never_types_outside_the_prompt() {
    let mock = demo(vec![], AfterEnter::Done);
    mock.state.lock().unwrap().screen = vec![
        " Do you trust the files in this folder?".into(),
        " ❯ 1. Yes, proceed".into(),
        "   2. No, exit".into(),
    ];
    let output = mock.rexctl(&["send", "demo", "hi", "--clear-input"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(mock.writes().is_empty());

    // An input box that won't clear: give up after a few tries, and send nothing.
    let mock = demo(vec![], AfterEnter::Done);
    {
        let mut state = mock.state.lock().unwrap();
        state.input = "stuck".into();
        state.sticky_input = true;
    }
    let output = mock.rexctl(&["send", "demo", "hi", "--clear-input"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(stderr(&output).contains("couldn't clear"), "{output:?}");
    let writes = mock.writes();
    assert!(
        writes.iter().all(|w| w.2 == "\x05" || w.2 == "\x15"),
        "{writes:?}"
    );
    assert!(writes.len() <= 2 * 4, "{writes:?}");
}

#[test]
fn send_rejects_bad_messages_before_writing() {
    let mock = demo(vec![], AfterEnter::Done);
    for message in ["up\x1b[A", "a\rb", "\x03"] {
        let output = mock.rexctl(&["send", "demo", message]);
        assert_eq!(code(&output), 3, "{message:?}: {output:?}");
    }
    let big = "x".repeat(40 * 1024);
    let output = mock.rexctl(&["send", "demo", &big]);
    assert_eq!(code(&output), 3);
    let output = mock.rexctl(&["send", "demo", "four", "--max-bytes", "3"]);
    assert_eq!(code(&output), 3);
    // --max-bytes can only lower the configured limit.
    let output = mock.rexctl(&["send", "demo", &big, "--max-bytes", "1000000"]);
    assert_eq!(code(&output), 3);
    assert!(mock.writes().is_empty());
    let audit = mock.audit();
    assert_eq!(audit.len(), 6);
    assert!(
        audit
            .iter()
            .all(|e| e["result"].as_str().unwrap().starts_with("refused:"))
    );
}

#[test]
fn endpoints_outside_the_allowlist_are_never_contacted() {
    let mock = demo(vec![], AfterEnter::Done);
    mock.write_config(&["https://rex-host.example.ts.net"]);
    for args in [&["list"][..], &["send", "demo", "hi"]] {
        let output = mock.rexctl(args);
        assert_eq!(code(&output), 3, "{args:?}: {output:?}");
    }
    assert_eq!(mock.connections.load(Ordering::SeqCst), 0);
    assert_eq!(mock.audit().len(), 1, "the refused send is audited");
}

#[test]
fn the_endpoint_comes_from_rex_server_or_config() {
    let mock = demo(vec![], AfterEnter::Done);
    let run = |extra_config: &str, env: Option<&str>| {
        std::fs::write(
            mock.config_path(),
            format!(
                "allowed_endpoints = [{:?}]\n{extra_config}",
                mock.endpoint()
            ),
        )
        .unwrap();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_rexctl"));
        command
            .arg("--config")
            .arg(mock.config_path())
            .args(["--json", "list"])
            .env_remove("REX_SERVER")
            .env("HOME", mock.dir.path());
        if let Some(env) = env {
            command.env("REX_SERVER", env);
        }
        command.output().unwrap()
    };
    assert_eq!(code(&run("", None)), 1, "no endpoint anywhere");
    assert_eq!(code(&run("", Some(&mock.endpoint()))), 0);
    let from_config = format!("endpoint = {:?}\n", mock.endpoint());
    assert_eq!(code(&run(&from_config, None)), 0);
}

#[test]
fn wait_returns_the_current_stop_status() {
    for (records, exit) in [
        (vec![record("done", None, None)], 0),
        (vec![record("blocked", Some("permission"), None)], 10),
        (vec![record("blocked", None, None)], 12),
        (vec![record("error", None, Some("crashed"))], 14),
    ] {
        let mock = demo(records, AfterEnter::Done);
        let output = mock.rexctl(&["wait", "demo", "--quiet"]);
        assert_eq!(code(&output), exit, "{output:?}");
        assert!(mock.writes().is_empty());
        assert!(mock.audit().is_empty(), "wait is read-only");
    }
    let mock = demo(vec![record("working", None, None)], AfterEnter::Done);
    let output = mock.rexctl(&["wait", "demo", "--timeout", "1s"]);
    assert_eq!(code(&output), 15, "{output:?}");
}

#[test]
fn new_creates_a_session_and_audits() {
    let mock = demo(vec![], AfterEnter::Done);
    let output = mock.rexctl(&[
        "--json",
        "new",
        "fix-ci",
        "--cwd",
        "/srv/app",
        "--cmd",
        "claude --verbose --append-system-prompt 'Be terse.'",
        "--purpose",
        "Fix the CI pipeline",
    ]);
    assert_eq!(code(&output), 0, "{output:?}");
    let event = &json_lines(&output)[0];
    assert_eq!(event["type"], "created");
    assert_eq!(event["label"], "fix-ci");
    assert_eq!(event["session_id"], "session:new");
    assert_eq!(event["block_id"], "block:new");
    assert_eq!(event["purpose"], "Fix the CI pipeline");

    let owned = mock
        .registry()
        .find(&mock.canonical_endpoint(), &"session:new".into())
        .unwrap()
        .expect("recorded as owned");
    assert_eq!(owned.label.as_deref(), Some("fix-ci"));
    assert_eq!(owned.purpose.as_deref(), Some("Fix the CI pipeline"));
    assert!(owned.created_at.ends_with('Z'));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(mock.registry().path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let created = mock.state.lock().unwrap().created.clone();
    assert_eq!(created[0]["label"], "fix-ci");
    let block = &created[0]["initial_windows"][0]["layout"]["block"];
    assert_eq!(block["flavor"], "com.superlogical.terminal.shell");
    assert_eq!(block["options"]["cwd"], "/srv/app");
    assert_eq!(
        block["options"]["command"],
        json!(["claude", "--verbose", "--append-system-prompt", "Be terse."])
    );

    let audit = mock.audit();
    assert_eq!(audit[0]["command"], "new");
    assert_eq!(audit[0]["result"], "created");
    assert_eq!(audit[0]["session"], "session:new");

    let again = mock.rexctl(&["new", "fix-ci"]);
    assert_eq!(code(&again), 3, "duplicate label: {again:?}");
    let bad = mock.rexctl(&["new", "x", "--purpose", "two\nlines"]);
    assert_eq!(code(&bad), 3, "{bad:?}");
    let bad = mock.rexctl(&["new", "y", "--cmd", "claude 'unclosed"]);
    assert_eq!(code(&bad), 2, "{bad:?}");
    assert_eq!(mock.state.lock().unwrap().created.len(), 1);

    // Sessions rexctl created can be sent to and closed, whatever Claude Code renames them.
    mock.rename("session:new", "CI pipeline fix");
    let output = mock.rexctl(&["send", "CI pipeline fix", "go", "--quiet"]);
    assert_eq!(code(&output), 0, "{output:?}");
    let output = mock.rexctl(&["close", "session:new"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert!(
        mock.registry()
            .find(&mock.canonical_endpoint(), &"session:new".into())
            .unwrap()
            .is_none()
    );
}

#[test]
fn new_without_a_label_lets_rex_name_the_session() {
    let mock = demo(vec![], AfterEnter::Done);
    let output = mock.rexctl(&["new", "--cwd", "/srv/app"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(stdout(&output), "session:new\t\tblock:new\n");
    let created = mock.state.lock().unwrap().created.clone();
    assert!(created[0].get("label").is_none(), "{created:?}");
    let owned = mock
        .registry()
        .find(&mock.canonical_endpoint(), &"session:new".into())
        .unwrap()
        .expect("recorded as owned");
    assert_eq!(owned.label, None);
    let raw: Value =
        serde_json::from_str(&std::fs::read_to_string(mock.registry().path()).unwrap()).unwrap();
    let entry = raw["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == "session:new")
        .unwrap();
    assert!(entry.get("label").is_none(), "{entry}");
    assert!(mock.audit()[0].get("session_label").is_none());
}

#[test]
fn list_status_and_read() {
    let mock = demo(
        vec![record("working", None, Some("Editing files"))],
        AfterEnter::Done,
    );

    let output = mock.rexctl(&["--json", "list"]);
    assert_eq!(code(&output), 0, "{output:?}");
    let sessions = json_lines(&output);
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0]["label"], "demo");
    assert_eq!(sessions[0]["owned"], true);
    assert_eq!(sessions[0]["purpose"], "demo the relay");
    assert_eq!(sessions[0]["created_label"], "original");
    assert_eq!(sessions[1]["owned"], false);
    assert!(sessions[1]["purpose"].is_null());

    // Ownership is shown by id, so a renamed session stays marked and a lookalike isn't.
    mock.rename(SID, "Renamed");
    mock.rename(FOREIGN, "lookalike");
    let output = mock.rexctl(&["list"]);
    assert_eq!(
        stdout(&output),
        format!("* {SID}\tRenamed\tdemo the relay\n  {FOREIGN}\tlookalike\n")
    );
    mock.rename(SID, "demo");

    let output = mock.rexctl(&["--json", "status", "demo"]);
    assert_eq!(code(&output), 0, "{output:?}");
    let status = &json_lines(&output)[0];
    assert_eq!(status["block_id"], claude(vec![]).id);
    assert_eq!(status["status"], "working");
    assert_eq!(status["msg"], "Editing files");
    assert_eq!(status["program"], "claude");

    let output = mock.rexctl(&["status", SID]);
    assert!(
        stdout(&output).contains("Working — Editing files"),
        "{output:?}"
    );

    let output = mock.rexctl(&["read", "demo", "--lines", "2"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(stdout(&output), format!("{}\n{}\n", BOX[2], BOX[3]));
}

#[test]
fn tree_shows_tabs_panes_and_status() {
    let mock = demo(
        vec![record("working", None, Some("Editing files"))],
        AfterEnter::Done,
    );
    let output = mock.rexctl(&["--json", "tree"]);
    assert_eq!(code(&output), 0, "{output:?}");
    let sessions = json_lines(&output);
    assert_eq!(sessions.len(), 2);
    let demo = &sessions[0];
    assert_eq!(demo["label"], "demo");
    assert_eq!(demo["owned"], true);
    let tab = &demo["tabs"][0];
    assert_eq!(
        (tab["tab"].clone(), tab["label"].clone()),
        (json!(1), json!("main"))
    );
    let panes = tab["panes"].as_array().unwrap();
    assert_eq!(panes[0]["label"], "shell");
    assert_eq!(panes[0]["program"], "bash");
    assert_eq!(panes[0]["status"], Value::Null);
    assert_eq!(panes[1]["pane"], 2);
    assert_eq!(panes[1]["claude"], true);
    assert_eq!(panes[1]["status"], "working");
    assert_eq!(panes[1]["msg"], "Editing files");

    let output = mock.rexctl(&["tree", "demo"]);
    let text = stdout(&output);
    assert!(
        text.starts_with(&format!("* demo  {SID}  (demo the relay)\n")),
        "{text}"
    );
    assert!(text.contains("    1  main  window:1  [active]\n"), "{text}");
    assert!(
        text.contains(&format!(
            "        1:2  claude  {}  claude  Working: Editing files  [focused]",
            claude(vec![]).id
        )),
        "{text}"
    );
    assert!(!text.contains("work"), "only the named session: {text}");
}

#[test]
fn targets_name_tabs_and_panes() {
    let mock = demo(vec![record("done", None, None)], AfterEnter::Done);
    let block_of = |target: &str| {
        let output = mock.rexctl(&["--json", "status", target]);
        assert_eq!(code(&output), 0, "{target}: {output:?}");
        json_lines(&output)[0]["block_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (shell_id, claude_id) = (shell().id, claude(vec![]).id);
    // A named pane is used as given, even when it isn't Claude Code.
    assert_eq!(block_of("demo:shell"), shell_id);
    assert_eq!(block_of("demo:main:shell"), shell_id);
    assert_eq!(block_of("demo:1:1"), shell_id);
    // A tab or a whole session picks its Claude Code pane.
    assert_eq!(block_of("demo:main"), claude_id);
    assert_eq!(block_of("demo:1"), claude_id);
    assert_eq!(block_of(&format!("{SID}:1:2")), claude_id);
    assert_eq!(block_of("demo"), claude_id);

    let output = mock.rexctl(&["status", "demo:nowhere"]);
    assert_eq!(code(&output), 1, "{output:?}");
    assert!(
        stderr(&output).contains("no tab or terminal pane"),
        "{output:?}"
    );

    // Ownership is still per session: a pane in a foreign session is refused.
    let output = mock.rexctl(&["send", "work:main:claude", "hi"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(stderr(&output).contains("--allow-foreign"), "{output:?}");
    // The named shell pane isn't at Claude Code's prompt, so send refuses it.
    let output = mock.rexctl(&["send", "demo:shell", "hi"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(mock.writes().is_empty());
    let output = mock.rexctl(&["send", "demo:claude", "hi", "--quiet"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert!(mock.writes().iter().all(|w| w.1 == claude_id));
}

/// The demo session, with the shared screen showing a shell prompt.
fn shell_demo() -> Mock {
    let mock = demo(vec![], AfterEnter::Done);
    mock.state.lock().unwrap().screen = vec!["$ ls".into(), "src".into(), "$ ".into()];
    mock
}

#[test]
fn run_types_into_an_idle_shell_and_prints_the_new_output() {
    let mock = shell_demo();
    mock.state.lock().unwrap().busy_after_enter = 3;
    let output = mock.rexctl(&["run", "demo", "--", "cargo", "test", "--quiet"]);
    assert_eq!(code(&output), 0, "{output:?}");
    // The session's idle shell, not its Claude Code pane.
    let shell_id = shell().id;
    assert_eq!(
        mock.writes(),
        [
            (SID.into(), shell_id.into(), "cargo test --quiet".into()),
            (SID.into(), shell_id.into(), "\r".into()),
        ]
    );
    let text = stdout(&output);
    assert!(
        text.contains("$ cargo test --quiet\noutput of cargo test --quiet\n"),
        "{text}"
    );
    assert!(!text.contains("src"), "the baseline isn't repeated: {text}");
    assert!(stderr(&output).ends_with("rexctl: Done\n"), "{output:?}");

    let audit = mock.audit();
    assert_eq!(audit.len(), 2);
    for entry in &audit {
        assert_eq!(entry["command"], "run");
        assert_eq!(entry["block"], shell_id);
        assert_eq!(entry["msg_len"], 18);
    }
    assert_eq!(audit[1]["result"], "done");

    // --quiet prints the tail at the end instead.
    let output = mock.rexctl(&[
        "run",
        "demo:shell",
        "--quiet",
        "--lines",
        "2",
        "--",
        "echo hi",
    ]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(stdout(&output), "output of echo hi\n$\n");
}

#[test]
fn run_refuses_panes_that_arent_idle_at_a_shell_prompt() {
    let mock = shell_demo();
    // Claude Code's pane, named directly.
    let output = mock.rexctl(&["run", "demo:claude", "--", "ls"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(
        stderr(&output).contains("isn't idle at a shell prompt"),
        "{output:?}"
    );
    // The shell is busy running something.
    mock.state.lock().unwrap().shell_busy = usize::MAX;
    let output = mock.rexctl(&["run", "demo:shell", "--", "ls"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(
        stderr(&output).contains("foreground program is make"),
        "{output:?}"
    );
    // No idle shell anywhere in the session.
    let output = mock.rexctl(&["run", "demo", "--", "ls"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(
        stderr(&output).contains("no pane here is idle"),
        "{output:?}"
    );
    // Foreign sessions, and commands that aren't one plain line.
    let output = mock.rexctl(&["run", "work", "--", "ls"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(stderr(&output).contains("--allow-foreign"), "{output:?}");
    for bad in ["ls\nrm -rf /", "ls\t", "\x03"] {
        let output = mock.rexctl(&["run", "demo", "--", bad]);
        assert_eq!(code(&output), 3, "{bad:?}: {output:?}");
    }
    assert!(mock.writes().is_empty());
}

#[test]
fn run_waits_for_the_prompt_and_times_out() {
    let mock = shell_demo();
    mock.state.lock().unwrap().busy_after_enter = usize::MAX;
    let output = mock.rexctl(&["--json", "run", "demo", "--timeout", "1s", "--", "make"]);
    assert_eq!(code(&output), 15, "{output:?}");
    let last = json_lines(&output).pop().unwrap();
    assert_eq!(last["status"], "timeout");
    assert_eq!(mock.writes().len(), 2, "typed once");
    assert_eq!(mock.audit().last().unwrap()["result"], "timeout");
}

#[test]
fn panes_without_claude_need_block() {
    let mock = Mock::start(vec![(SID, "shell", vec![shell()])], AfterEnter::Done);
    mock.own(SID, None);
    let output = mock.rexctl(&["status", "shell"]);
    assert_eq!(code(&output), 1, "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--block"));

    let output = mock.rexctl(&["--json", "status", "shell", "--block", "5he11"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(json_lines(&output)[0]["block_id"], shell().id);

    // The shell never turned on bracketed paste, so send won't type into it.
    let output = mock.rexctl(&["send", "shell", "ls", "--block", "5he11"]);
    assert_eq!(code(&output), 3, "{output:?}");
    assert!(mock.writes().is_empty());
}

#[test]
fn usage_errors_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let output = rexctl(
        dir.path(),
        &dir.path().join("c.toml"),
        "http://127.0.0.1:9",
        &["send"],
    );
    assert_eq!(code(&output), 2);
}
