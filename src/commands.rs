//! The commands.

use std::io::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rexrs::api::session::Session;
use serde_json::{Value, json};

use crate::audit::{self, AuditLog, Entry};
use crate::cli::{Cli, Command, SendArgs, Target, WaitArgs, WaitOptions};
use crate::config::{self, Config};
use crate::endpoint::Endpoint;
use crate::exit;
use crate::guard::{self, Refusal};
use crate::output::{DEFAULT_HOLDBACK, Pump};
use crate::pane::{self, Evidence, Pane};
use crate::registry::{Owned, Registry};
use crate::rex::Rex;
use crate::status::{self, Records, Status, Tracker};

/// How often `send` and `wait` re-read the status in case an event was missed.
const STATUS_POLL: Duration = Duration::from_secs(5);
/// The pause between pasting a message and pressing Enter, so the program has taken in the
/// paste before Enter arrives.
const ENTER_DELAY: Duration = Duration::from_millis(250);

/// Runs the command and returns the process exit code.
pub fn run(cli: &Cli) -> i32 {
    let out = Out { json: cli.json };
    match execute(cli, &out) {
        Ok(code) => code,
        Err(error) => {
            let code = if error.downcast_ref::<Refusal>().is_some() {
                exit::REFUSED
            } else {
                exit::ERROR
            };
            if out.json {
                out.event(
                    json!({"type": "error", "message": format!("{error:#}"), "exit_code": code}),
                );
            } else {
                eprintln!("rexctl: {error:#}");
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
        eprintln!("rexctl: {text}");
    }
}

struct Ctx {
    config: Config,
    endpoint: Endpoint,
    audit: AuditLog,
    registry: Registry,
}

impl Ctx {
    fn connect(&self) -> Result<Rex> {
        Rex::connect(&self.endpoint)
    }

    /// The endpoint in canonical form, as the registry and the audit log record it.
    fn endpoint_key(&self) -> String {
        self.endpoint.to_string()
    }

    fn owned(&self, session_id: &str) -> Result<Option<Owned>> {
        self.registry.find(&self.endpoint_key(), session_id)
    }
}

fn execute(cli: &Cli, out: &Out) -> Result<i32> {
    let config_path = config::config_path(cli.config.as_deref())?;
    let config = Config::load(&config_path)?;
    let audit = AuditLog::new(config.audit_log()?);
    let registry = Registry::new(config.registry()?);
    let mutating = match cli.command {
        Command::New { .. } => Some("new"),
        Command::Send(_) => Some("send"),
        Command::Close { .. } => Some("close"),
        _ => None,
    };
    if mutating.is_some() {
        audit.ensure_writable()?;
    }
    if matches!(cli.command, Command::New { .. } | Command::Close { .. }) {
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
        Command::Status(target) => show_status(&ctx, out, target),
        Command::Read { target, lines } => read(&ctx, out, target, *lines),
        Command::New {
            label,
            cwd,
            cmd,
            purpose,
        } => new(&ctx, out, label, cwd.as_deref(), cmd, purpose.as_deref()),
        Command::Send(args) => send(&ctx, out, args),
        Command::Wait(args) => wait(&ctx, out, args),
        Command::Close { session } => close(&ctx, out, session),
    }
}

/// A session and the pane the relay acts on in it.
struct Located {
    session: Session,
    pane: Pane,
    evidence: Evidence,
}

/// Finds the target pane in a resolved session. `attach` joins the session (for its events);
/// otherwise the connection only observes.
fn locate(rex: &mut Rex, session: Session, target: &Target, attach: bool) -> Result<Located> {
    let view = if attach {
        rex.attach(&session.session_id)?.1
    } else {
        rex.observe_all()?;
        rex.view(&session.session_id)?
    };
    let panes = pane::panes(&view);
    let sid = session.session_id.clone();
    // A pane that can't answer one of these simply offers no evidence.
    let mut gather = |pane: &Pane| Evidence {
        records: rex
            .program_status(&sid, &pane.block_id)
            .map(|r| r.1)
            .unwrap_or_default(),
        process: rex.process(&sid, &pane.block_id).ok(),
        title: rex.title(&sid, &pane.block_id).ok(),
    };
    if let Some(wanted) = &target.block {
        let pane = pane::find_block(&panes, wanted)?.clone();
        let evidence = gather(&pane);
        return Ok(Located {
            session,
            pane,
            evidence,
        });
    }
    let candidates: Vec<_> = panes
        .into_iter()
        .map(|pane| {
            let evidence = gather(&pane);
            (pane, evidence)
        })
        .collect();
    let (pane, evidence) = pane::choose_claude(&candidates)?.clone();
    Ok(Located {
        session,
        pane,
        evidence,
    })
}

fn list(ctx: &Ctx, out: &Out) -> Result<i32> {
    let mut rex = ctx.connect()?;
    let sessions = rex.sessions()?;
    rex.close();
    let owned = ctx.registry.on(&ctx.endpoint_key())?;
    for s in &sessions {
        let mine = owned.iter().find(|o| o.session_id == s.session_id);
        if out.json {
            out.event(json!({
                "type": "session",
                "session_id": s.session_id,
                "label": s.label,
                "owned": mine.is_some(),
                "purpose": mine.and_then(|o| o.purpose.as_deref()),
                "created_label": mine.map(|o| o.label.as_str()),
                "created_at": mine.map(|o| o.created_at.as_str()),
            }));
        } else {
            let mark = if mine.is_some() { "*" } else { " " };
            let mut line = format!("{mark} {}\t{}", s.session_id, s.label);
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
    let session = rex.resolve(&target.session)?;
    let located = locate(&mut rex, session, target, false)?;
    rex.close();
    let status = located.evidence.records.status();
    let msg = located.evidence.records.msg();
    if out.json {
        out.event(json!({
            "type": "status",
            "session_id": located.session.session_id,
            "session_label": located.session.label,
            "block_id": located.pane.block_id,
            "status": status,
            "label": status.label(),
            "msg": msg,
            "program": located.evidence.program(),
            "claude": located.evidence.claude_reason(),
        }));
    } else {
        let mut line = format!(
            "{} ({}) {}: {}",
            located.session.label,
            located.session.session_id,
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
    let session = rex.resolve(&target.session)?;
    let located = locate(&mut rex, session, target, false)?;
    let text = rex.read_text(&located.session.session_id, &located.pane.block_id)?;
    rex.close();
    let lines = last_lines(&text, count);
    if out.json {
        out.event(json!({
            "type": "text",
            "session_id": located.session.session_id,
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

fn new(
    ctx: &Ctx,
    out: &Out,
    label: &str,
    cwd: Option<&str>,
    cmd: &str,
    purpose: Option<&str>,
) -> Result<i32> {
    let label = guard::prefixed_label(label.trim(), ctx.config.label_prefix());
    let purpose = purpose.map(str::trim);
    let mut entry = Entry::new("new", ctx.endpoint_key());
    entry.session_label = Some(label.clone());
    let result = create(ctx, &label, cwd, cmd, purpose, &mut entry);
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
        out.line(&format!("{session_id}\t{label}\t{block_id}"));
    }
    Ok(exit::OK)
}

fn create(
    ctx: &Ctx,
    label: &str,
    cwd: Option<&str>,
    cmd: &str,
    purpose: Option<&str>,
    entry: &mut Entry,
) -> Result<(String, String)> {
    guard::check_label(label)?;
    if let Some(purpose) = purpose {
        guard::check_purpose(purpose)?;
    }
    let command: Vec<String> = cmd.split_whitespace().map(str::to_owned).collect();
    if command.is_empty() {
        bail!("--cmd is empty");
    }
    if let Some(cwd) = cwd
        && (cwd.is_empty() || cwd.chars().any(char::is_control))
    {
        return Err(Refusal("--cwd must be a plain path".into()).into());
    }
    let mut rex = ctx.connect()?;
    if rex.sessions()?.iter().any(|s| s.label == label) {
        return Err(Refusal(format!("a session labelled {label:?} already exists")).into());
    }
    let created = rex.create(label, cwd, &command)?;
    rex.close();
    entry.session = Some(created.session_id.clone());
    // Record ownership before anything else can fail: an unrecorded session could never be
    // sent to without --allow-foreign, nor closed.
    ctx.registry
        .add(Owned {
            endpoint: ctx.endpoint_key(),
            session_id: created.session_id.clone(),
            label: label.to_owned(),
            created_at: audit::now(),
            purpose: purpose.map(str::to_owned),
        })
        .with_context(|| {
            format!(
                "session {} was created but couldn't be recorded as owned",
                created.session_id
            )
        })?;
    let block_id = created
        .initial_windows
        .first()
        .and_then(|w| w.block_ids.first())
        .cloned()
        .context("the server created no block")?;
    entry.block = Some(block_id.clone());
    Ok((created.session_id, block_id))
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

/// Closes `wanted` if, and only if, the registry says rexctl created it on this endpoint.
/// The ownership check happens before any close request is sent.
fn close_owned(ctx: &Ctx, wanted: &str, entry: &mut Entry) -> Result<Closed> {
    let mut rex = ctx.connect()?;
    let session = match rex.resolve(wanted) {
        Ok(session) => session,
        Err(error) => {
            // A registered id the server no longer knows: forget it, close nothing.
            let gone = !rex.sessions()?.iter().any(|s| s.session_id == wanted);
            if gone && ctx.owned(wanted)?.is_some() {
                rex.close();
                ctx.registry.remove(&ctx.endpoint_key(), wanted)?;
                return Ok(Closed::AlreadyGone);
            }
            return Err(error);
        }
    };
    entry.session = Some(session.session_id.clone());
    entry.session_label = Some(session.label.clone());
    let owned = ctx.owned(&session.session_id)?.is_some();
    guard::check_close_owned(owned, &session.session_id, &session.label)?;
    rex.observe_all()?;
    rex.destroy(&session.session_id)?;
    rex.close();
    ctx.registry
        .remove(&ctx.endpoint_key(), &session.session_id)
        .with_context(|| {
            format!(
                "closed {} but couldn't remove it from the registry",
                session.session_id
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
    let mut entry = Entry::new("send", ctx.endpoint_key()).message(&args.message);
    entry.session = Some(args.target.session.clone());
    let mut delivered = false;
    let result = deliver(ctx, out, args, &mut entry, &mut delivered);
    // A delivered message already has its "sent" entry; this one records how it ended.
    if delivered {
        entry.timestamp = audit::now();
    }
    entry.result = match &result {
        Ok(outcome) => outcome.as_str().to_owned(),
        Err(error) => audit_error(error),
    };
    ctx.audit.append(&entry)?;
    let outcome = result?;
    Ok(finish(out, outcome))
}

fn deliver(
    ctx: &Ctx,
    out: &Out,
    args: &SendArgs,
    entry: &mut Entry,
    delivered: &mut bool,
) -> Result<Outcome> {
    // The flag can only tighten the configured limit.
    let limit = ctx.config.max_message_bytes();
    let max = args.max_bytes.map_or(limit, |m| m.min(limit));
    guard::check_message(&args.message, max)?;
    let mut rex = ctx.connect()?;
    // The label is resolved now, as Claude Code may have renamed the session; ownership is
    // decided by its id alone, before the session is even attached.
    let session = rex.resolve(&args.target.session)?;
    entry.session = Some(session.session_id.clone());
    entry.session_label = Some(session.label.clone());
    let owned = ctx.owned(&session.session_id)?.is_some();
    guard::check_send_owned(
        owned,
        &session.session_id,
        &session.label,
        args.allow_foreign,
    )?;
    let located = locate(&mut rex, session, &args.target, true)?;
    let (sid, bid) = (
        located.session.session_id.clone(),
        located.pane.block_id.clone(),
    );
    entry.block = Some(bid.clone());

    let (seq, records) = rex.program_status(&sid, &bid)?;
    let initial = records.status();
    guard::check_status_for_send(initial, args.answer)?;
    if rex.bracketed_paste(&sid, &bid).ok().flatten() == Some(false) {
        return Err(Refusal(
            "the pane hasn't turned on bracketed paste, so it isn't at Claude Code's prompt; use --block if this is the wrong pane".into(),
        )
        .into());
    }
    let baseline = if args.wait.quiet {
        None
    } else {
        Some(rex.read_text(&sid, &bid)?)
    };

    rex.write(&sid, &bid, &format!("\x1b[200~{}\x1b[201~", args.message))?;
    std::thread::sleep(ENTER_DELAY);
    rex.write(&sid, &bid, "\r")?;
    *delivered = true;
    let mut sent = entry.clone();
    sent.result = "sent".into();
    ctx.audit.append(&sent)?;

    if out.json {
        out.event(json!({
            "type": "sent",
            "session_id": sid,
            "session_label": located.session.label,
            "block_id": bid,
            "bytes": args.message.len(),
        }));
    } else {
        out.note(&format!(
            "sent {} bytes to {} {bid}",
            args.message.len(),
            located.session.label
        ));
    }
    let watch = Watch {
        sid,
        bid,
        records,
        seq,
        tracker: Tracker::after_send(initial),
        pump: baseline.map(|b| Pump::new(&b, DEFAULT_HOLDBACK)),
    };
    let outcome = watch.run(&mut rex, out, &args.wait, timeout(ctx, &args.wait));
    rex.close();
    outcome
}

fn wait(ctx: &Ctx, out: &Out, args: &WaitArgs) -> Result<i32> {
    let mut rex = ctx.connect()?;
    let session = rex.resolve(&args.target.session)?;
    let located = locate(&mut rex, session, &args.target, true)?;
    let (sid, bid) = (located.session.session_id, located.pane.block_id);
    let (seq, records) = rex.program_status(&sid, &bid)?;
    let tracker = Tracker::waiting(records.status());
    let pump = if args.wait.quiet {
        None
    } else {
        Some(Pump::new(&rex.read_text(&sid, &bid)?, DEFAULT_HOLDBACK))
    };
    let watch = Watch {
        sid,
        bid,
        records,
        seq,
        tracker,
        pump,
    };
    let outcome = watch.run(&mut rex, out, &args.wait, timeout(ctx, &args.wait));
    rex.close();
    Ok(finish(out, outcome?))
}

fn timeout(ctx: &Ctx, options: &WaitOptions) -> Duration {
    options.timeout.unwrap_or(ctx.config.timeout())
}

/// How `send` or `wait` ended.
#[derive(Debug, Clone)]
enum Outcome {
    Stopped { status: Status, msg: Option<String> },
    TimedOut { status: Status },
}

impl Outcome {
    fn as_str(&self) -> &'static str {
        match self {
            Outcome::Stopped { status, .. } => status.as_str(),
            Outcome::TimedOut { .. } => "timeout",
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
    }
}

/// Follows one pane until its program stops: applies its status events, streams its text.
struct Watch {
    sid: String,
    bid: String,
    records: Records,
    /// Events up to this seq are already reflected in `records`.
    seq: u64,
    tracker: Tracker,
    pump: Option<Pump>,
}

impl Watch {
    fn run(
        mut self,
        rex: &mut Rex,
        out: &Out,
        options: &WaitOptions,
        timeout: Duration,
    ) -> Result<Outcome> {
        let start = Instant::now();
        let deadline = start + timeout;
        let poll = options.poll.max(Duration::from_millis(100));
        let mut next_read = start + poll;
        let mut next_status = start + STATUS_POLL;
        let mut shown = self.tracker.current();
        if out.json {
            out.event(self.status_event(shown));
        }
        let mut finished = self.tracker.settled_now();
        while finished.is_none() {
            let now = Instant::now();
            if now >= deadline {
                self.flush(rex, out)?;
                return Ok(Outcome::TimedOut {
                    status: self.tracker.current(),
                });
            }
            if self.pump.is_some() && now >= next_read {
                let text = rex.read_text(&self.sid, &self.bid)?;
                if let Some(pump) = &mut self.pump {
                    print_lines(out, pump.update(&text));
                }
                next_read = now + poll;
            }
            if now >= next_status {
                let (seq, fresh) = rex.program_status(&self.sid, &self.bid)?;
                self.seq = self.seq.max(seq);
                if fresh != self.records {
                    self.records = fresh;
                    finished = self.changed(out, &mut shown);
                }
                next_status = now + STATUS_POLL;
                continue;
            }
            let mut until = deadline.min(next_status);
            if self.pump.is_some() {
                until = until.min(next_read);
            }
            let wait = until
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(10));
            if let Some(event) = rex.next_event(wait)? {
                finished = self.handle(&event, out, &mut shown)?;
            }
        }
        self.flush(rex, out)?;
        let status = finished.unwrap_or_default();
        Ok(Outcome::Stopped {
            status,
            msg: self.records.msg().map(str::to_owned),
        })
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
            } else if !status.is_blocked() && status != Status::Error {
                // Blocked and error states are reported once, at the end.
                out.note(status.label());
            }
        }
        self.tracker.observe(status)
    }

    fn status_event(&self, status: Status) -> Value {
        json!({
            "type": "status",
            "status": status,
            "label": status.label(),
            "msg": self.records.msg(),
        })
    }

    /// Prints whatever output is left.
    fn flush(&mut self, rex: &mut Rex, out: &Out) -> Result<()> {
        if self.pump.is_none() {
            return Ok(());
        }
        let text = rex.read_text(&self.sid, &self.bid)?;
        if let Some(pump) = &mut self.pump {
            print_lines(out, pump.finish(&text));
        }
        Ok(())
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
