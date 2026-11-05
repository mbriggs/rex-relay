//! The commands.

use std::io::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::audit::{self, AuditLog, Entry};
use crate::cli::{
    Cli, Command, NewArgs, Program, RunArgs, SendArgs, Target, WaitArgs, WaitOptions,
};
use crate::config::{self, Config};
use crate::endpoint::Endpoint;
use crate::exit;
use crate::guard::{self, Refusal};
use crate::ids::{BlockId, SessionId};
use crate::output::{DEFAULT_HOLDBACK, Pump};
use crate::pane::{self, Evidence, Pane, Scope};
use crate::prompt;
use crate::registry::{Owned, Registry};
use crate::rex::{self, Ambiguous, Rex, Session, Unreachable};
use crate::status::{self, Records, Status, Tracker};

/// How often `send` and `wait` re-read the status, events or not. This catches a missed
/// event, and the round trip is also what notices a connection that went quiet.
const STATUS_POLL: Duration = Duration::from_secs(3);
/// The pause between pasting a message and pressing Enter, so the program has taken in the
/// paste before Enter arrives.
const ENTER_DELAY: Duration = Duration::from_millis(250);
/// The pause after a clearing keystroke, so Claude Code has redrawn its input box.
const KEY_DELAY: Duration = Duration::from_millis(150);
/// Clearing keystroke pairs `--clear-input` may send before it gives up. Each pair clears one
/// line or joins an empty line to the one above.
const MAX_CLEAR_STEPS: usize = 40;
/// How often `new --wait-ready` reads the new pane.
const READY_POLL: Duration = Duration::from_millis(500);
/// How many of the pane's last lines `new --wait-ready` prints when it times out.
const READY_LINES: usize = 20;

/// Runs the command and returns the process exit code.
pub fn run(cli: &Cli) -> i32 {
    let out = Out { json: cli.json };
    match execute(cli, &out) {
        Ok(code) => code,
        Err(error) => {
            let code = if error.is::<Refusal>() {
                exit::REFUSED
            } else if error.is::<Unreachable>() {
                exit::LOST_CONNECTION
            } else if error.is::<Ambiguous>() {
                exit::USAGE
            } else {
                exit::ERROR
            };
            if out.json {
                out.event(
                    json!({"type": "error", "message": format!("{error:#}"), "exit_code": code}),
                );
            } else {
                eprintln!("rex-relay: {error:#}");
            }
            code
        }
    }
}

struct Out {
    json: bool,
}

impl Out {
    fn line(&self, line: &str) {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }

    fn event(&self, value: Value) {
        self.line(&value.to_string());
    }

    fn note(&self, text: &str) {
        eprintln!("rex-relay: {text}");
    }
}

struct Ctx {
    config: Config,
    endpoint: Endpoint,
    audit: AuditLog,
    registry: Registry,
}

impl Ctx {
    /// Connects, retrying a connection that can't be made, such as during a network blip, for
    /// up to `connect_timeout`. Each failed attempt is noted on stderr.
    fn connect(&self) -> Result<Rex> {
        let started = Instant::now();
        let patience = self.config.connect_timeout();
        let mut failures = 0;
        loop {
            let error = match Rex::connect(&self.endpoint, self.config.call_timeout(), None) {
                Ok(rex) => return Ok(rex),
                Err(error) if rex::is_connection_error(&error) => error,
                Err(error) => return Err(error),
            };
            failures += 1;
            let pause = rex::backoff(failures);
            if started.elapsed() + pause > patience {
                return Err(Unreachable {
                    endpoint: self.endpoint_key(),
                    attempts: failures,
                    waited: started.elapsed(),
                    last: format!("{error:#}"),
                }
                .into());
            }
            eprintln!(
                "rex-relay: connection attempt {failures} to {} failed: {error:#}; retrying in {}s",
                self.endpoint,
                pause.as_secs()
            );
            std::thread::sleep(pause);
        }
    }

    /// The endpoint in canonical form, as the registry and the audit log record it.
    fn endpoint_key(&self) -> String {
        self.endpoint.to_string()
    }

    fn owned(&self, session_id: &SessionId) -> Result<Option<Owned>> {
        self.registry.find(&self.endpoint_key(), session_id)
    }

    /// Every session rex-relay created on this endpoint.
    fn all_owned(&self) -> Result<Vec<Owned>> {
        self.registry.on(&self.endpoint_key())
    }
}

fn execute(cli: &Cli, out: &Out) -> Result<i32> {
    let config_path = config::config_path(cli.config.as_deref())?;
    let config = Config::load(&config_path)?;
    let audit = AuditLog::new(config.audit_log()?);
    let registry = Registry::new(config.registry()?);
    let mutating = match cli.command {
        Command::New(_) => Some("new"),
        Command::Send(_) => Some("send"),
        Command::Run(_) => Some("run"),
        Command::Close { .. } => Some("close"),
        _ => None,
    };
    if mutating.is_some() {
        audit.ensure_writable()?;
    }
    if matches!(cli.command, Command::New(_) | Command::Close { .. }) {
        registry.ensure_writable()?;
    }

    let env = std::env::var(config::SERVER_ENV).ok();
    let (address, _source) =
        config::choose_endpoint(cli.endpoint.as_deref(), env.as_deref(), &config)?;
    let endpoint = Endpoint::parse(&address)?;
    if let Err(refusal) = guard::check_endpoint(&endpoint, &config.allowed_endpoints) {
        if let Some(command) = mutating {
            let mut entry = Entry::new(command, endpoint.to_string());
            entry.result = refusal.to_string();
            audit.append(&entry)?;
        }
        return Err(refusal.into());
    }
    let ctx = Ctx {
        config,
        endpoint,
        audit,
        registry,
    };
    match &cli.command {
        Command::List => list(&ctx, out),
        Command::Tree { session } => tree(&ctx, out, session.as_deref()),
        Command::Status(target) => show_status(&ctx, out, target),
        Command::Read { target, lines } => read(&ctx, out, target, *lines),
        Command::New(args) => new(&ctx, out, args),
        Command::Send(args) => send(&ctx, out, args),
        Command::Wait(args) => wait(&ctx, out, args),
        Command::Run(args) => run_shell(&ctx, out, args),
        Command::Close { session } => close(&ctx, out, session),
    }
}

/// A session and the pane the relay acts on in it.
struct Located {
    session: Session,
    pane: Pane,
    evidence: Evidence,
}

/// Picks one pane among candidates, by what runs in them.
type Chooser = fn(&[(Pane, Evidence)]) -> Result<&(Pane, Evidence)>;

/// What a pane says about its program. A pane that can't answer one of these simply offers no
/// evidence.
fn gather(rex: &mut Rex, sid: &SessionId, pane: &Pane) -> Evidence {
    Evidence {
        records: rex
            .program_status(sid, &pane.block_id)
            .map(|r| r.1)
            .unwrap_or_default(),
        process: rex.process(sid, &pane.block_id).ok(),
        title: rex.title(sid, &pane.block_id).ok(),
    }
}

/// Finds the target pane in a resolved session: the panes `scope` names, narrowed by `block`,
/// or else picked by `choose`. `attach` joins the session (for its events); otherwise the
/// connection only observes.
fn locate(
    rex: &mut Rex,
    session: Session,
    scope: &Scope,
    block: Option<&str>,
    attach: bool,
    choose: Chooser,
) -> Result<Located> {
    let view = if attach {
        rex.attach(&session.id)?.1
    } else {
        rex.observe_all()?;
        rex.view(&session.id)?
    };
    let panes = pane::panes(&view);
    let selection = pane::select(&panes, scope)?;
    let sid = session.id.clone();
    let given = match block {
        Some(wanted) => Some(pane::find_block(&selection.panes, wanted)?.clone()),
        None if selection.named && selection.panes.len() == 1 => Some(selection.panes[0].clone()),
        None => None,
    };
    if let Some(pane) = given {
        let evidence = gather(rex, &sid, &pane);
        return Ok(Located {
            session,
            pane,
            evidence,
        });
    }
    let candidates: Vec<_> = selection
        .panes
        .into_iter()
        .map(|pane| {
            let evidence = gather(rex, &sid, &pane);
            (pane, evidence)
        })
        .collect();
    let (pane, evidence) = choose(&candidates)?.clone();
    Ok(Located {
        session,
        pane,
        evidence,
    })
}

/// Resolves a command's target to a session and the pane in it that `choose` picks.
fn find(
    ctx: &Ctx,
    rex: &mut Rex,
    target: &Target,
    attach: bool,
    choose: Chooser,
) -> Result<Located> {
    let (session, scope) = rex.resolve_target(&target.name, &ctx.all_owned()?)?;
    locate(
        rex,
        session,
        &scope,
        target.block.as_deref(),
        attach,
        choose,
    )
}

fn tree(ctx: &Ctx, out: &Out, only: Option<&str>) -> Result<i32> {
    let mut rex = ctx.connect()?;
    let owned = ctx.all_owned()?;
    let sessions = match only {
        Some(name) => vec![rex.resolve(name, &owned)?],
        None => rex.sessions()?,
    };
    rex.observe_all()?;
    for session in &sessions {
        let view = rex.view(&session.id)?;
        let panes: Vec<(Pane, Evidence)> = pane::panes(&view)
            .into_iter()
            .map(|pane| {
                let evidence = gather(&mut rex, &session.id, &pane);
                (pane, evidence)
            })
            .collect();
        let mine = owned.iter().find(|o| o.session_id == session.id);
        if out.json {
            out.event(tree_json(session, mine, &panes));
        } else {
            print_tree(out, session, mine, &panes);
        }
    }
    rex.close();
    if !out.json && sessions.is_empty() {
        out.note("no sessions");
    }
    Ok(exit::OK)
}

/// A pane's status for listings: its label and message, or nothing without status records.
fn pane_status(evidence: &Evidence) -> Option<String> {
    if evidence.records.is_empty() {
        return None;
    }
    let status = evidence.records.status();
    Some(match evidence.records.msg() {
        Some(msg) => format!("{}: {msg}", status.label()),
        None => status.label().to_owned(),
    })
}

fn print_tree(out: &Out, session: &Session, mine: Option<&Owned>, panes: &[(Pane, Evidence)]) {
    let mark = if mine.is_some() { "*" } else { " " };
    let mut line = format!("{mark} {}  {}", session.label, session.id);
    if let Some(purpose) = mine.and_then(|o| o.purpose.as_deref()) {
        line.push_str(&format!("  ({purpose})"));
    }
    out.line(&line);
    let mut tab = 0;
    for (pane, evidence) in panes {
        if pane.tab != tab {
            tab = pane.tab;
            let mut line = format!(
                "    {tab}  {}  {}",
                pane.window_label.as_deref().unwrap_or("-"),
                pane.window_id.as_deref().unwrap_or("-")
            );
            if pane.tab_active {
                line.push_str("  [active]");
            }
            out.line(&line);
        }
        let mut line = format!(
            "        {tab}:{}  {}  {}  {}  {}",
            pane.index,
            pane.label.as_deref().unwrap_or("-"),
            pane.block_id,
            evidence.program().as_deref().unwrap_or("-"),
            pane_status(evidence).as_deref().unwrap_or("-"),
        );
        if pane.focused {
            line.push_str("  [focused]");
        }
        out.line(&line);
    }
}

fn tree_json(session: &Session, mine: Option<&Owned>, panes: &[(Pane, Evidence)]) -> Value {
    let mut tabs: Vec<Value> = Vec::new();
    for (pane, evidence) in panes {
        if tabs.len() < pane.tab {
            tabs.push(json!({
                "tab": pane.tab,
                "window_id": pane.window_id,
                "label": pane.window_label,
                "active": pane.tab_active,
                "panes": [],
            }));
        }
        let status = (!evidence.records.is_empty()).then(|| evidence.records.status());
        tabs[pane.tab - 1]["panes"]
            .as_array_mut()
            .expect("panes is an array")
            .push(json!({
                "pane": pane.index,
                "block_id": pane.block_id,
                "label": pane.label,
                "focused": pane.focused,
                "program": evidence.program(),
                "claude": evidence.claude_reason().is_some(),
                "status": status,
                "msg": evidence.records.msg(),
            }));
    }
    json!({
        "type": "session",
        "session_id": session.id,
        "label": session.label,
        "owned": mine.is_some(),
        "purpose": mine.and_then(|o| o.purpose.as_deref()),
        "tabs": tabs,
    })
}

fn list(ctx: &Ctx, out: &Out) -> Result<i32> {
    let mut rex = ctx.connect()?;
    let sessions = rex.sessions()?;
    rex.close();
    let owned = ctx.registry.on(&ctx.endpoint_key())?;
    for s in &sessions {
        let mine = owned.iter().find(|o| o.session_id == s.id);
        if out.json {
            out.event(json!({
                "type": "session",
                "session_id": s.id,
                "label": s.label,
                "owned": mine.is_some(),
                "purpose": mine.and_then(|o| o.purpose.as_deref()),
                "created_label": mine.and_then(|o| o.label.as_deref()),
                "created_at": mine.map(|o| o.created_at.as_str()),
            }));
        } else {
            let mark = if mine.is_some() { "*" } else { " " };
            let mut line = format!("{mark} {}\t{}", s.id, s.label);
            // The label from `new` still resolves, so show it once Claude Code renames the session.
            if let Some(created) = mine.and_then(|o| o.label.as_deref())
                && created != s.label
            {
                line.push_str(&format!(" (created as {created})"));
            }
            if let Some(purpose) = mine.and_then(|o| o.purpose.as_deref()) {
                line.push_str(&format!("\t{purpose}"));
            }
            out.line(&line);
        }
    }
    if !out.json && sessions.is_empty() {
        out.note("no sessions");
    }
    Ok(exit::OK)
}

fn show_status(ctx: &Ctx, out: &Out, target: &Target) -> Result<i32> {
    let mut rex = ctx.connect()?;
    let located = find(ctx, &mut rex, target, false, pane::choose_claude)?;
    let (sid, bid) = (&located.session.id, &located.pane.block_id);
    let mut status = located.evidence.records.status();
    let reading = match status {
        Status::Idle => Some(prompt::read(&rex.screen_vt(sid, bid)?)),
        _ => None,
    };
    rex.close();
    let dialog = reading.as_ref().and_then(|r| r.startup_dialog(status));
    let summary = dialog.map(prompt::Dialog::summary);
    if dialog.is_some() {
        status = Status::Dialog;
    }
    let msg = summary.as_deref().or(located.evidence.records.msg());
    if out.json {
        out.event(json!({
            "type": "status",
            "session_id": located.session.id,
            "session_label": located.session.label,
            "block_id": located.pane.block_id,
            "status": status,
            "label": status.label(),
            "msg": msg,
            "dialog": dialog.map(|d| &d.text),
            "folder": dialog.and_then(|d| d.folder.as_deref()),
            "program": located.evidence.program(),
            "claude": located.evidence.claude_reason(),
        }));
    } else {
        let mut line = format!(
            "{} ({}) {}: {}",
            located.session.label,
            located.session.id,
            located.pane.block_id,
            status.label()
        );
        if let Some(msg) = msg {
            line.push_str(&format!(" — {msg}"));
        }
        out.line(&line);
    }
    Ok(exit::OK)
}

fn read(ctx: &Ctx, out: &Out, target: &Target, count: usize) -> Result<i32> {
    let mut rex = ctx.connect()?;
    let located = find(ctx, &mut rex, target, false, pane::choose_claude)?;
    let text = rex.read_text(&located.session.id, &located.pane.block_id)?;
    rex.close();
    let lines = last_lines(&text, count);
    if out.json {
        out.event(json!({
            "type": "text",
            "session_id": located.session.id,
            "block_id": located.pane.block_id,
            "lines": lines,
        }));
    } else {
        for line in lines {
            out.line(line);
        }
    }
    Ok(exit::OK)
}

/// The last `count` lines of `text` (all of them for 0), without trailing blank lines.
fn last_lines(text: &str, count: usize) -> Vec<&str> {
    let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if count > 0 && lines.len() > count {
        lines.drain(..lines.len() - count);
    }
    lines
}

fn new(ctx: &Ctx, out: &Out, args: &NewArgs) -> Result<i32> {
    let label = args.label.as_deref().map(str::trim);
    let purpose = args.purpose.as_deref().map(str::trim);
    let mut entry = Entry::new("new", ctx.endpoint_key());
    entry.session_label = label.map(str::to_owned);
    let result = create(
        ctx,
        label,
        args.cwd.as_deref(),
        &args.cmd,
        purpose,
        &mut entry,
    );
    entry.result = match &result {
        Ok(_) => "created".to_owned(),
        Err(error) => audit_error(error),
    };
    ctx.audit.append(&entry)?;
    let (session_id, block_id) = result?;
    if out.json {
        out.event(json!({
            "type": "created",
            "session_id": session_id,
            "label": label,
            "block_id": block_id,
            "purpose": purpose,
        }));
    } else {
        // The label column stays, empty without a label, so the columns don't shift.
        out.line(&format!(
            "{session_id}\t{}\t{block_id}",
            label.unwrap_or_default()
        ));
    }
    if !args.wait_ready {
        return Ok(exit::OK);
    }
    let timeout = args.timeout.unwrap_or(ctx.config.timeout());
    wait_ready(ctx, out, &session_id, &block_id, timeout)
}

/// Reads a new pane until `send` would type into it, or until it shows something a person has
/// to answer, such as the folder-trust dialog.
fn wait_ready(
    ctx: &Ctx,
    out: &Out,
    sid: &SessionId,
    bid: &BlockId,
    timeout: Duration,
) -> Result<i32> {
    let deadline = Instant::now() + timeout;
    let mut rex = ctx.connect()?;
    let outcome = loop {
        let records = rex.program_status(sid, bid)?.1;
        let status = records.status();
        let reading = prompt::read(&rex.screen_vt(sid, bid)?);
        if let Some(dialog) = reading.startup_dialog(status) {
            let msg = Some(dialog.text.clone());
            break Outcome::Stopped {
                status: Status::Dialog,
                msg,
            };
        }
        if status.is_blocked() || status == Status::Error {
            let msg = records.msg().map(str::to_owned);
            break Outcome::Stopped { status, msg };
        }
        let paste = rex.bracketed_paste(sid, bid).ok().flatten();
        if reading.input.is_none()
            && guard::check_sendable(status, &reading, paste, true, false).is_ok()
        {
            break Outcome::Stopped { status, msg: None };
        }
        if Instant::now() >= deadline {
            let text = rex.read_text(sid, bid)?;
            let lines = last_lines(&text, READY_LINES);
            print_lines(out, lines.into_iter().map(str::to_owned).collect());
            break Outcome::TimedOut { status };
        }
        std::thread::sleep(READY_POLL);
    };
    rex.close();
    Ok(finish(out, outcome))
}

fn create(
    ctx: &Ctx,
    label: Option<&str>,
    cwd: Option<&str>,
    cmd: &Program,
    purpose: Option<&str>,
    entry: &mut Entry,
) -> Result<(SessionId, BlockId)> {
    if let Some(label) = label {
        guard::check_label(label)?;
    }
    if let Some(purpose) = purpose {
        guard::check_purpose(purpose)?;
    }
    if let Some(cwd) = cwd
        && (cwd.is_empty() || cwd.chars().any(char::is_control))
    {
        return Err(Refusal("--cwd must be a plain path".into()).into());
    }
    let mut rex = ctx.connect()?;
    // A duplicate label would make the label ambiguous for every later command. Owned
    // sessions keep answering to the label they were created with, so that counts too.
    if let Some(label) = label
        && let owned = ctx.all_owned()?
        && rex.sessions()?.iter().any(|s| s.answers_to(label, &owned))
    {
        return Err(Refusal(format!("a session labelled {label:?} already exists")).into());
    }
    // Rex starts a pane in the home directory when its cwd doesn't exist, so check first.
    let cwd = cwd.map(|cwd| rex.resolve_dir(cwd)).transpose()?;
    let created = rex.create(label, cwd.as_deref(), &cmd.0)?;
    rex.close();
    entry.session = Some(created.session.to_string());
    // Record ownership before anything else can fail: an unrecorded session could never be
    // sent to without --allow-foreign, nor closed.
    ctx.registry
        .add(Owned {
            endpoint: ctx.endpoint_key(),
            session_id: created.session.clone(),
            label: label.map(str::to_owned),
            created_at: audit::now(),
            purpose: purpose.map(str::to_owned),
        })
        .with_context(|| {
            format!(
                "session {} was created but couldn't be recorded as owned",
                created.session
            )
        })?;
    let block_id = created.block.context("the server created no block")?;
    entry.block = Some(block_id.to_string());
    Ok((created.session, block_id))
}

fn close(ctx: &Ctx, out: &Out, wanted: &str) -> Result<i32> {
    let mut entry = Entry::new("close", ctx.endpoint_key());
    entry.session = Some(wanted.to_owned());
    let result = close_owned(ctx, wanted, &mut entry);
    entry.result = match &result {
        Ok(Closed::Closed) => "closed".to_owned(),
        Ok(Closed::AlreadyGone) => "already gone; removed from registry".to_owned(),
        Err(error) => audit_error(error),
    };
    ctx.audit.append(&entry)?;
    let closed = result?;
    let session = entry.session.unwrap_or_default();
    let gone = matches!(closed, Closed::AlreadyGone);
    if out.json {
        out.event(json!({"type": "closed", "session_id": session, "already_gone": gone}));
    } else if gone {
        out.note(&format!(
            "{session} no longer exists; removed it from the registry"
        ));
    } else {
        out.line(&format!("closed {session}"));
    }
    Ok(exit::OK)
}

enum Closed {
    Closed,
    /// The session was registered but the server no longer has it.
    AlreadyGone,
}

/// Closes `wanted` if, and only if, the registry says rex-relay created it on this endpoint.
/// The ownership check happens before any close request is sent.
fn close_owned(ctx: &Ctx, wanted: &str, entry: &mut Entry) -> Result<Closed> {
    let mut rex = ctx.connect()?;
    let session = match rex.resolve(wanted, &ctx.all_owned()?) {
        Ok(session) => session,
        Err(error) => {
            // A registered id the server no longer knows: forget it, close nothing.
            let wanted = SessionId::from(wanted);
            let gone = !rex.sessions()?.iter().any(|s| s.id == wanted);
            if gone && ctx.owned(&wanted)?.is_some() {
                rex.close();
                ctx.registry.remove(&ctx.endpoint_key(), &wanted)?;
                return Ok(Closed::AlreadyGone);
            }
            return Err(error);
        }
    };
    entry.session = Some(session.id.to_string());
    entry.session_label = Some(session.label.clone());
    let owned = ctx.owned(&session.id)?.is_some();
    guard::check_close_owned(owned, session.id.as_str(), &session.label)?;
    rex.observe_all()?;
    rex.destroy(&session.id)?;
    rex.close();
    ctx.registry
        .remove(&ctx.endpoint_key(), &session.id)
        .with_context(|| {
            format!(
                "closed {} but couldn't remove it from the registry",
                session.id
            )
        })?;
    Ok(Closed::Closed)
}

fn audit_error(error: &anyhow::Error) -> String {
    match error.downcast_ref::<Refusal>() {
        Some(refusal) => refusal.to_string(),
        None => format!("error: {error:#}"),
    }
}

fn send(ctx: &Ctx, out: &Out, args: &SendArgs) -> Result<i32> {
    let attempt = Attempt::new("send", &args.message, &args.target.name);
    audited(ctx, out, attempt, |a| a.send(ctx, out, args))
}

fn run_shell(ctx: &Ctx, out: &Out, args: &RunArgs) -> Result<i32> {
    let command = args.command.join(" ");
    let attempt = Attempt::new("run", &command, &args.target.name);
    audited(ctx, out, attempt, |a| a.run(ctx, out, args, &command))
}

/// Runs `act` and appends the audit entry for how it ended, whatever happened.
fn audited<'a>(
    ctx: &Ctx,
    out: &Out,
    mut attempt: Attempt<'a>,
    act: impl FnOnce(&mut Attempt<'a>) -> Result<Outcome>,
) -> Result<i32> {
    let result = act(&mut attempt);
    let outcome = match &result {
        Ok(outcome) => outcome.as_str().to_owned(),
        Err(error) => audit_error(error),
    };
    ctx.audit.append(&attempt.entry(ctx, outcome))?;
    Ok(finish(out, result?))
}

/// One `send` or `run`, and what it has learned about its target so far, which the audit log
/// records however far it got.
struct Attempt<'a> {
    /// `send` or `run`.
    command: &'static str,
    /// The message or command line. The audit log keeps only its hash and length.
    text: &'a str,
    /// The target as given, until it resolves to a session.
    target: &'a str,
    started: String,
    session: Option<Session>,
    block: Option<BlockId>,
    /// The message was typed into the pane.
    sent: bool,
    /// Unsent text was cleared from the input box first.
    input_cleared: bool,
}

impl<'a> Attempt<'a> {
    fn new(command: &'static str, text: &'a str, target: &'a str) -> Self {
        Attempt {
            command,
            text,
            target,
            started: audit::now(),
            session: None,
            block: None,
            sent: false,
            input_cleared: false,
        }
    }

    /// An audit entry with this result. After the message is sent, entries carry the time
    /// they are written; before, the time the attempt began.
    fn entry(&self, ctx: &Ctx, result: String) -> Entry {
        let mut entry = Entry::new(self.command, ctx.endpoint_key()).message(self.text);
        if !self.sent {
            entry.timestamp.clone_from(&self.started);
        }
        entry.session = Some(match &self.session {
            Some(session) => session.id.to_string(),
            None => self.target.to_owned(),
        });
        entry.session_label = self.session.as_ref().map(|s| s.label.clone());
        entry.block = self.block.as_ref().map(BlockId::to_string);
        entry.input_cleared = self.input_cleared;
        entry.result = result;
        entry
    }

    fn send(&mut self, ctx: &Ctx, out: &Out, args: &SendArgs) -> Result<Outcome> {
        // The flag can only tighten the configured limit.
        let limit = ctx.config.max_message_bytes();
        let max = args.max_bytes.map_or(limit, |m| m.min(limit));
        guard::check_message(&args.message, max)?;
        let mut rex = ctx.connect()?;
        // The label is resolved now, as Claude Code may have renamed the session; ownership is
        // decided by its id alone, before the session is even attached.
        let (session, scope) = rex.resolve_target(&args.target.name, &ctx.all_owned()?)?;
        self.session = Some(session.clone());
        let owned = ctx.owned(&session.id)?.is_some();
        guard::check_send_owned(
            owned,
            session.id.as_str(),
            &session.label,
            args.allow_foreign,
        )?;
        let located = locate(
            &mut rex,
            session,
            &scope,
            args.target.block.as_deref(),
            true,
            pane::choose_claude,
        )?;
        let (sid, bid) = (located.session.id, located.pane.block_id);
        self.block = Some(bid.clone());

        let (seq, records) = rex.program_status(&sid, &bid)?;
        let initial = records.status();
        let reading = prompt::read(&rex.screen_vt(&sid, &bid)?);
        let paste = rex.bracketed_paste(&sid, &bid).ok().flatten();
        let claude = located.evidence.claude_reason().is_some();
        guard::check_sendable(initial, &reading, paste, claude, args.answer)?;
        if let Some(input) = reading.input {
            if !args.clear_input {
                return Err(guard::leftover_input(&input).into());
            }
            clear_input(&mut rex, &sid, &bid)?;
            self.input_cleared = true;
        }
        let screen = rex.read_text(&sid, &bid)?;

        rex.write(&sid, &bid, &format!("\x1b[200~{}\x1b[201~", args.message))?;
        std::thread::sleep(ENTER_DELAY);
        rex.write(&sid, &bid, "\r")?;
        self.sent = true;
        ctx.audit.append(&self.entry(ctx, "sent".into()))?;

        if out.json {
            out.event(json!({
                "type": "sent",
                "session_id": sid,
                "session_label": located.session.label,
                "block_id": bid,
                "bytes": args.message.len(),
            }));
        } else if !args.wait.quiet {
            out.note(&format!(
                "sent {} bytes to {} {bid}",
                args.message.len(),
                located.session.label
            ));
        }
        let pump = Output::new(&args.wait, DEFAULT_HOLDBACK, || Ok(screen))?;
        let watch = Watch {
            sid,
            bid,
            records,
            seq,
            tracker: Tracker::after_send(initial),
            pump,
        };
        watch.run(rex, ctx, out, &args.wait)
    }
}

impl Attempt<'_> {
    fn run(&mut self, ctx: &Ctx, out: &Out, args: &RunArgs, command: &str) -> Result<Outcome> {
        let limit = ctx.config.max_message_bytes();
        let max = args.max_bytes.map_or(limit, |m| m.min(limit));
        guard::check_command(command, max)?;
        let mut rex = ctx.connect()?;
        let (session, scope) = rex.resolve_target(&args.target.name, &ctx.all_owned()?)?;
        self.session = Some(session.clone());
        let owned = ctx.owned(&session.id)?.is_some();
        guard::check_send_owned(
            owned,
            session.id.as_str(),
            &session.label,
            args.allow_foreign,
        )?;
        let located = locate(
            &mut rex,
            session,
            &scope,
            args.target.block.as_deref(),
            true,
            pane::choose_shell,
        )?;
        let (sid, bid) = (located.session.id, located.pane.block_id);
        self.block = Some(bid.clone());
        if located.evidence.idle_shell().is_none() {
            let program = located
                .evidence
                .program()
                .unwrap_or_else(|| "unknown".into());
            return Err(Refusal(format!(
                "the pane isn't idle at a shell prompt (its foreground program is {program})"
            ))
            .into());
        }
        let output = Output::new(&args.wait, SHELL_HOLDBACK, || rex.read_text(&sid, &bid))?;
        rex.write(&sid, &bid, command)?;
        std::thread::sleep(KEY_DELAY);
        let typed = rex.read_text(&sid, &bid)?;
        rex.write(&sid, &bid, "\r")?;
        self.sent = true;
        ctx.audit.append(&self.entry(ctx, "sent".into()))?;
        if out.json {
            out.event(json!({
                "type": "sent",
                "session_id": sid,
                "session_label": located.session.label,
                "block_id": bid,
                "bytes": command.len(),
            }));
        } else if !args.wait.quiet {
            out.note(&format!(
                "ran {} bytes in {} {bid}",
                command.len(),
                located.session.label
            ));
        }
        let watch = ShellWatch {
            sid,
            bid,
            typed,
            output,
        };
        let mut link = Link::new(rex, ctx);
        let result = watch.follow(&mut link, out, &args.wait, timeout(ctx, &args.wait));
        link.close();
        result
    }
}

/// Lines at the bottom of a shell's screen that may still change: the prompt line.
const SHELL_HOLDBACK: usize = 1;
/// How often `run` checks whether the shell is back at its prompt.
const SHELL_POLL: Duration = Duration::from_millis(250);

/// Follows a shell pane after `run` pressed Enter, until the shell is back at its prompt. Rex
/// reports no command boundaries or exit statuses, so the prompt counts as back when the shell
/// is the foreground program and the screen has changed since the command was typed, on two
/// checks in a row. It only reads, so nothing it does after a reconnect can type.
struct ShellWatch {
    sid: SessionId,
    bid: BlockId,
    /// The pane's text with the command typed, before Enter.
    typed: String,
    output: Output,
}

impl ShellWatch {
    fn follow(
        mut self,
        link: &mut Link,
        out: &Out,
        options: &WaitOptions,
        timeout: Duration,
    ) -> Result<Outcome> {
        let deadline = Instant::now() + timeout;
        let poll = options.poll.clamp(Duration::from_millis(100), SHELL_POLL);
        let mut back_once = false;
        loop {
            let now = Instant::now();
            if now >= deadline {
                let status = Status::Working;
                if link.rex.is_none() {
                    return Ok(Outcome::LostConnection { status });
                }
                self.flush(link, out);
                return Ok(Outcome::TimedOut { status });
            }
            let Some(rex) = link.rex.as_mut() else {
                if link.gave_up(now) {
                    return Ok(Outcome::LostConnection {
                        status: Status::Working,
                    });
                }
                if now < link.retry_at {
                    link.pause(now, deadline);
                    continue;
                }
                let attempt = Rex::connect(link.endpoint, link.call_timeout, link.token.as_deref())
                    .and_then(|mut rex| rex.observe_all().map(|()| rex));
                match attempt {
                    Ok(rex) => {
                        link.restore(rex);
                        connection_note(out, "restored", "reconnected");
                    }
                    Err(error) => {
                        link.failed();
                        if link.failures == 1 {
                            connection_note(out, "retrying", &format!("{error:#}"));
                        }
                    }
                }
                continue;
            };
            match self.back_at_prompt(rex, out) {
                Ok(true) if back_once => break,
                Ok(back) => back_once = back,
                Err(error) if rex::is_connection_error(&error) => {
                    connection_note(out, "lost", &format!("{error:#}"));
                    link.lose();
                    back_once = false;
                    continue;
                }
                Err(error) => return Err(error),
            }
            std::thread::sleep(poll.min(deadline.saturating_duration_since(Instant::now())));
        }
        self.flush(link, out);
        Ok(Outcome::Stopped {
            status: Status::Done,
            msg: None,
        })
    }

    /// Reads the pane, prints new output, and says whether the shell looks back at its prompt.
    fn back_at_prompt(&mut self, rex: &mut Rex, out: &Out) -> Result<bool> {
        let text = rex.read_text(&self.sid, &self.bid)?;
        self.output.update(out, &text);
        let evidence = Evidence {
            process: Some(rex.process(&self.sid, &self.bid)?),
            ..Evidence::default()
        };
        Ok(evidence.idle_shell().is_some() && text.trim_end() != self.typed.trim_end())
    }

    fn flush(&mut self, link: &mut Link, out: &Out) {
        let Some(rex) = link.rex.as_mut() else {
            return;
        };
        match rex.read_text(&self.sid, &self.bid) {
            Ok(text) => self.output.finish(out, &text),
            Err(error) => out.note(&format!("couldn't read the pane's last output: {error:#}")),
        }
    }
}

/// Clears unsent text from Claude Code's input box with Ctrl+E (end of line) and Ctrl+U (delete
/// to the start of the line, or the line break before it). Both only edit the input. Ctrl+C
/// would also clear it, but interrupts Claude Code while it works. Each step re-reads the screen
/// and stops at once if the pane is no longer at the prompt.
fn clear_input(rex: &mut Rex, sid: &SessionId, bid: &BlockId) -> Result<()> {
    let mut last = None;
    let mut stuck = 0;
    for _ in 0..MAX_CLEAR_STEPS {
        for key in ["\x05", "\x15"] {
            rex.write(sid, bid, key)?;
            std::thread::sleep(KEY_DELAY);
        }
        let reading = prompt::read(&rex.screen_vt(sid, bid)?);
        guard::check_ready(&reading, None, true)?;
        let Some(input) = reading.input else {
            return Ok(());
        };
        if last.as_ref() == Some(&input) {
            stuck += 1;
            if stuck == 3 {
                break;
            }
        } else {
            stuck = 0;
        }
        last = Some(input);
    }
    Err(Refusal("couldn't clear Claude Code's input box; nothing was sent".into()).into())
}

fn wait(ctx: &Ctx, out: &Out, args: &WaitArgs) -> Result<i32> {
    let mut rex = ctx.connect()?;
    let located = find(ctx, &mut rex, &args.target, true, pane::choose_claude)?;
    let (sid, bid) = (located.session.id, located.pane.block_id);
    let (seq, records) = rex.program_status(&sid, &bid)?;
    // A startup dialog leaves the records idle, so only the screen tells it from a ready
    // prompt. Its text is the message: a person has to read it to answer.
    if records.status() == Status::Idle
        && let Some(dialog) = prompt::read(&rex.screen_vt(&sid, &bid)?).startup_dialog(Status::Idle)
    {
        let msg = Some(dialog.text.clone());
        rex.close();
        return Ok(finish(
            out,
            Outcome::Stopped {
                status: Status::Dialog,
                msg,
            },
        ));
    }
    let tracker = Tracker::waiting(records.status());
    let pump = Output::new(&args.wait, DEFAULT_HOLDBACK, || rex.read_text(&sid, &bid))?;
    let watch = Watch {
        sid,
        bid,
        records,
        seq,
        tracker,
        pump,
    };
    let outcome = watch.run(rex, ctx, out, &args.wait);
    Ok(finish(out, outcome?))
}

fn timeout(ctx: &Ctx, options: &WaitOptions) -> Duration {
    options.timeout.unwrap_or(ctx.config.timeout())
}

/// How `send` or `wait` ended.
#[derive(Debug, Clone)]
enum Outcome {
    Stopped {
        status: Status,
        msg: Option<String>,
    },
    TimedOut {
        status: Status,
    },
    /// The connection broke and couldn't be restored before the timeout.
    LostConnection {
        status: Status,
    },
}

impl Outcome {
    fn as_str(&self) -> &'static str {
        match self {
            Outcome::Stopped { status, .. } => status.as_str(),
            Outcome::TimedOut { .. } => "timeout",
            Outcome::LostConnection { .. } => "lost_connection",
        }
    }
}

fn finish(out: &Out, outcome: Outcome) -> i32 {
    match outcome {
        Outcome::Stopped { status, msg } => {
            let code = status.exit_code();
            if out.json {
                out.event(json!({
                    "type": "result",
                    "status": status,
                    "label": status.label(),
                    "msg": msg,
                    "exit_code": code,
                }));
            } else {
                match msg {
                    Some(msg) => out.note(&format!("{}: {msg}", status.label())),
                    None => out.note(status.label()),
                }
            }
            code
        }
        Outcome::TimedOut { status } => {
            if out.json {
                out.event(json!({
                    "type": "result",
                    "status": "timeout",
                    "label": "Timed out",
                    "last_status": status,
                    "exit_code": exit::TIMEOUT,
                }));
            } else {
                out.note(&format!("timed out (last status: {})", status.label()));
            }
            exit::TIMEOUT
        }
        Outcome::LostConnection { status } => {
            if out.json {
                out.event(json!({
                    "type": "result",
                    "status": "lost_connection",
                    "label": "Lost connection",
                    "last_status": status,
                    "exit_code": exit::LOST_CONNECTION,
                }));
            } else {
                out.note(&format!(
                    "lost the connection to the server and couldn't reconnect (last status: {})",
                    status.label()
                ));
            }
            exit::LOST_CONNECTION
        }
    }
}

/// A watch's connection to the server, which the watch replaces when it stalls or breaks.
struct Link<'a> {
    endpoint: &'a Endpoint,
    call_timeout: Duration,
    /// How long a lost connection may take to come back before the watch gives up.
    patience: Duration,
    rex: Option<Rex>,
    /// Continues the same client on the server after a reconnect.
    token: Option<String>,
    /// When the connection was lost, while it is down.
    lost_at: Option<Instant>,
    /// Failed reconnect attempts since the connection was lost.
    failures: u32,
    /// When to try to reconnect next.
    retry_at: Instant,
}

impl<'a> Link<'a> {
    fn new(rex: Rex, ctx: &'a Ctx) -> Self {
        Link {
            endpoint: &ctx.endpoint,
            call_timeout: ctx.config.call_timeout(),
            patience: ctx.config.connect_timeout(),
            token: rex.reconnect_token(),
            rex: Some(rex),
            lost_at: None,
            failures: 0,
            retry_at: Instant::now(),
        }
    }

    /// Drops a connection that stalled or broke. There's no close handshake, because it would
    /// wait on the server. The first reconnect attempt is immediate.
    fn lose(&mut self) {
        let now = Instant::now();
        self.rex = None;
        self.lost_at = Some(now);
        self.failures = 0;
        self.retry_at = now;
    }

    /// The connection has been down for longer than `patience`.
    fn gave_up(&self, now: Instant) -> bool {
        self.lost_at.is_some_and(|lost| now >= lost + self.patience)
    }

    /// Waits for the next reconnect attempt, but no later than `deadline` or the end of
    /// `patience`.
    fn pause(&self, now: Instant, deadline: Instant) {
        let mut until = self.retry_at.min(deadline);
        if let Some(lost) = self.lost_at {
            until = until.min(lost + self.patience);
        }
        std::thread::sleep(until.saturating_duration_since(now));
    }

    fn failed(&mut self) {
        self.failures += 1;
        self.retry_at = Instant::now() + rex::backoff(self.failures);
    }

    fn restore(&mut self, rex: Rex) {
        if let Some(token) = rex.reconnect_token() {
            self.token = Some(token);
        }
        self.rex = Some(rex);
        self.lost_at = None;
        self.failures = 0;
    }

    fn close(self) {
        if let Some(rex) = self.rex {
            rex.close();
        }
    }
}

/// The server said the watched session or pane no longer exists. Reconnecting won't help.
#[derive(Debug)]
struct Gone(&'static str);

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Gone {}

/// Whether a failed reconnect means the server answered, so that retrying can't help.
fn server_answered(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<Gone>()
            || matches!(
                cause.downcast_ref::<rexrs::Error>(),
                Some(rexrs::Error::Api(_))
            )
    })
}

/// When the watch next reads the pane's text and status, and when it gives up.
struct Clock {
    deadline: Instant,
    poll: Duration,
    next_read: Instant,
    next_status: Instant,
}

/// Follows one pane until its program stops: applies its status events, streams its text.
/// It only reads. Nothing it does after a reconnect can type into the pane.
struct Watch {
    sid: SessionId,
    bid: BlockId,
    records: Records,
    /// Events up to this seq are already reflected in `records`.
    seq: u64,
    tracker: Tracker,
    pump: Output,
}

/// What a watch prints of the pane's text.
enum Output {
    /// New lines as they settle, starting from a baseline that counts as already seen.
    Stream(Pump),
    /// Only the last lines at the end (`--quiet`). 0 means every line.
    Tail(usize),
}

impl Output {
    /// `baseline` reads the pane's text, and is only called when streaming. `holdback` lines
    /// at the bottom wait until they stop changing.
    fn new(
        options: &WaitOptions,
        holdback: usize,
        baseline: impl FnOnce() -> Result<String>,
    ) -> Result<Self> {
        Ok(if options.quiet {
            Output::Tail(options.lines)
        } else {
            Output::Stream(Pump::new(&baseline()?, holdback))
        })
    }

    /// Prints the new lines in `text`, when streaming.
    fn update(&mut self, out: &Out, text: &str) {
        if let Output::Stream(pump) = self {
            print_lines(out, pump.update(text));
        }
    }

    /// Prints what's left at the end: the rest of a stream, or the tail.
    fn finish(&mut self, out: &Out, text: &str) {
        match self {
            Output::Stream(pump) => print_lines(out, pump.finish(text)),
            Output::Tail(count) if out.json => out.event(json!({
                "type": "tail",
                "lines": last_lines(text, *count),
            })),
            Output::Tail(count) => {
                for line in last_lines(text, *count) {
                    out.line(line);
                }
            }
        }
    }

    fn streams(&self) -> bool {
        matches!(self, Output::Stream(_))
    }
}

impl Watch {
    fn run(self, rex: Rex, ctx: &Ctx, out: &Out, options: &WaitOptions) -> Result<Outcome> {
        let mut link = Link::new(rex, ctx);
        let result = self.follow(&mut link, out, options, timeout(ctx, options));
        link.close();
        result
    }

    fn follow(
        mut self,
        link: &mut Link,
        out: &Out,
        options: &WaitOptions,
        timeout: Duration,
    ) -> Result<Outcome> {
        let start = Instant::now();
        let poll = options.poll.max(Duration::from_millis(100));
        let mut clock = Clock {
            deadline: start + timeout,
            poll,
            next_read: start + poll,
            next_status: start + STATUS_POLL,
        };
        let mut shown = self.tracker.current();
        if out.json {
            out.event(self.status_event(shown));
        }
        let mut finished = self.tracker.settled_now();
        while finished.is_none() {
            let now = Instant::now();
            if now >= clock.deadline {
                let status = self.tracker.current();
                if link.rex.is_none() {
                    return Ok(Outcome::LostConnection { status });
                }
                self.flush(link, out);
                return Ok(Outcome::TimedOut { status });
            }
            let Some(rex) = link.rex.as_mut() else {
                if link.gave_up(now) {
                    return Ok(Outcome::LostConnection {
                        status: self.tracker.current(),
                    });
                }
                if now < link.retry_at {
                    link.pause(now, clock.deadline);
                    continue;
                }
                finished = self.resume(link, out, &mut shown)?;
                clock.next_status = Instant::now() + STATUS_POLL;
                continue;
            };
            match self.step(rex, &mut clock, out, &mut shown) {
                Ok(done) => finished = done.or_else(|| self.tracker.settled_by(Instant::now())),
                Err(error) if rex::is_connection_error(&error) => {
                    connection_note(out, "lost", &format!("{error:#}"));
                    link.lose();
                }
                Err(error) => return Err(error),
            }
        }
        self.flush(link, out);
        Ok(Outcome::Stopped {
            status: finished.unwrap_or_default(),
            msg: self.records.msg().map(str::to_owned),
        })
    }

    /// One turn on a live connection: read the text or the status if due, else wait for an
    /// event.
    fn step(
        &mut self,
        rex: &mut Rex,
        clock: &mut Clock,
        out: &Out,
        shown: &mut Status,
    ) -> Result<Option<Status>> {
        let now = Instant::now();
        if self.pump.streams() && now >= clock.next_read {
            let text = rex.read_text(&self.sid, &self.bid)?;
            self.pump.update(out, &text);
            clock.next_read = now + clock.poll;
        }
        if now >= clock.next_status {
            let (seq, fresh) = rex.program_status(&self.sid, &self.bid)?;
            clock.next_status = now + STATUS_POLL;
            self.seq = self.seq.max(seq);
            if fresh != self.records {
                self.records = fresh;
                return Ok(self.changed(out, shown));
            }
            return Ok(None);
        }
        let mut until = clock.deadline.min(clock.next_status);
        if self.pump.streams() {
            until = until.min(clock.next_read);
        }
        if let Some(idle) = self.tracker.idle_deadline() {
            until = until.min(idle);
        }
        let wait = until
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(10));
        match rex.next_event(wait)? {
            Some(event) => self.handle(&event, out, shown),
            None => Ok(None),
        }
    }

    /// Reconnects, joins the session again for its events, and re-reads the pane's status,
    /// since anything reported while disconnected was missed. A failed attempt schedules the
    /// next one. Errors only when the server says the session or pane is gone.
    fn resume(&mut self, link: &mut Link, out: &Out, shown: &mut Status) -> Result<Option<Status>> {
        let attempt = || -> Result<(Rex, u64, Records)> {
            let mut rex = Rex::connect(link.endpoint, link.call_timeout, link.token.as_deref())?;
            let (_, view) = rex.attach(&self.sid)?;
            if !pane::panes(&view).iter().any(|p| p.block_id == self.bid) {
                return Err(Gone("the pane was closed").into());
            }
            let (seq, records) = rex.program_status(&self.sid, &self.bid)?;
            Ok((rex, seq, records))
        };
        match attempt() {
            Ok((rex, seq, records)) => {
                link.restore(rex);
                connection_note(out, "restored", "reconnected");
                // Events from the new connection up to `seq` are already in `records`.
                self.seq = seq;
                if records != self.records {
                    self.records = records;
                    return Ok(self.changed(out, shown));
                }
                Ok(None)
            }
            Err(error) if server_answered(&error) => Err(error),
            Err(error) => {
                link.failed();
                if link.failures == 1 {
                    connection_note(out, "retrying", &format!("{error:#}"));
                }
                Ok(None)
            }
        }
    }

    fn handle(
        &mut self,
        event: &rexrs::Event,
        out: &Out,
        shown: &mut Status,
    ) -> Result<Option<Status>> {
        if event.session_id.as_deref() != Some(self.sid.as_str()) {
            if event.name == "session_destroyed"
                && event.payload["session_id"].as_str() == Some(self.sid.as_str())
            {
                bail!("the session was destroyed");
            }
            return Ok(None);
        }
        match event.name.as_str() {
            "block_closed" if event.payload["block_id"].as_str() == Some(self.bid.as_str()) => {
                bail!("the pane was closed")
            }
            "session_destroyed" => bail!("the session was destroyed"),
            "block_event" if event.seq > self.seq => {
                let Some((block, change)) = status::parse_block_event(&event.payload) else {
                    return Ok(None);
                };
                if block != self.bid {
                    return Ok(None);
                }
                self.seq = event.seq;
                if self.records.apply(&change) {
                    return Ok(self.changed(out, shown));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// The records changed after the watch began: report the new status and see if it ends
    /// the watch.
    fn changed(&mut self, out: &Out, shown: &mut Status) -> Option<Status> {
        let status = self.records.status();
        if status != *shown {
            *shown = status;
            if out.json {
                out.event(self.status_event(status));
            } else if self.pump.streams() && !status.is_blocked() && status != Status::Error {
                // Blocked and error states are reported once, at the end.
                out.note(status.label());
            }
        }
        self.tracker.observe(status, Instant::now())
    }

    fn status_event(&self, status: Status) -> Value {
        json!({
            "type": "status",
            "status": status,
            "label": status.label(),
            "msg": self.records.msg(),
        })
    }

    /// Prints whatever output is left, or the tail, if the connection is up. The outcome
    /// matters more than the last lines, so a failed read is only noted.
    fn flush(&mut self, link: &mut Link, out: &Out) {
        let Some(rex) = link.rex.as_mut() else {
            return;
        };
        let text = match rex.read_text(&self.sid, &self.bid) {
            Ok(text) => text,
            Err(error) => {
                out.note(&format!("couldn't read the pane's last output: {error:#}"));
                return;
            }
        };
        self.pump.finish(out, &text);
    }
}

/// Reports a change in the connection: `lost`, `retrying`, or `restored`.
fn connection_note(out: &Out, state: &str, message: &str) {
    if out.json {
        out.event(json!({"type": "connection", "state": state, "message": message}));
    } else {
        out.note(&format!("connection {state}: {message}"));
    }
}

fn print_lines(out: &Out, lines: Vec<String>) {
    for line in lines {
        if out.json {
            out.event(json!({"type": "output", "line": line}));
        } else {
            out.line(&line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_lines_trims_and_counts() {
        assert_eq!(last_lines("a\nb  \nc\n\n\n", 2), ["b", "c"]);
        assert_eq!(last_lines("a\nb", 0), ["a", "b"]);
        assert_eq!(last_lines("a\nb", 10), ["a", "b"]);
        assert!(last_lines("", 3).is_empty());
    }
}
