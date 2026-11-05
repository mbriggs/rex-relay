//! What a pane's program reports about itself with OSC 7501, folded into one status.
//!
//! Records arrive from `com.superlogical.terminal.program_status` and from the terminal's
//! `program_status_changed` / `program_status_removed` block events. A pane's status is its
//! highest record, in the order Blocked > Error > Working > Done > Idle.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::exit;
use crate::ids::BlockId;

pub const TERMINAL_CREATOR: &str = "com.superlogical.terminal";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum State {
    #[default]
    Idle,
    Done,
    Working,
    Error,
    Blocked,
}

impl State {
    /// Unknown states read as idle, so a newer server's states never raise an alarm.
    pub fn parse(value: &str) -> Self {
        match value {
            "working" => State::Working,
            "done" => State::Done,
            "blocked" => State::Blocked,
            "error" => State::Error,
            _ => State::Idle,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
}

impl BlockedKind {
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value? {
            "permission" => Some(BlockedKind::Permission),
            "question" => Some(BlockedKind::Question),
            "auth" => Some(BlockedKind::Auth),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub state: State,
    pub kind: Option<BlockedKind>,
    pub app: Option<String>,
    pub msg: Option<String>,
    /// When the program made the report. A program that finishes twice reports Done twice,
    /// so this is what tells the second Done from the first.
    pub updated_at: Option<String>,
}

impl Record {
    pub fn parse(value: &Value) -> Self {
        let text = |key: &str| {
            value[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        Record {
            state: State::parse(value["state"].as_str().unwrap_or_default()),
            kind: BlockedKind::parse(value["kind"].as_str()),
            app: text("effective_app").or_else(|| text("app")),
            msg: text("msg").or_else(|| text("title")),
            updated_at: text("updated_at"),
        }
    }
}

/// What the relay reports: the folded state, with a blocked state split by what it waits for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Status {
    #[default]
    Idle,
    Working,
    Done,
    NeedsApproval,
    Question,
    NeedsSignIn,
    NeedsInput,
    /// A menu the program doesn't report in its status, such as Claude Code's folder-trust
    /// dialog. Only the screen shows it.
    Dialog,
    Error,
}

impl Status {
    pub fn from_record(state: State, kind: Option<BlockedKind>) -> Self {
        match state {
            State::Idle => Status::Idle,
            State::Done => Status::Done,
            State::Working => Status::Working,
            State::Error => Status::Error,
            State::Blocked => match kind {
                Some(BlockedKind::Permission) => Status::NeedsApproval,
                Some(BlockedKind::Question) => Status::Question,
                Some(BlockedKind::Auth) => Status::NeedsSignIn,
                None => Status::NeedsInput,
            },
        }
    }

    /// The words Rex shows for the status.
    pub fn label(self) -> &'static str {
        match self {
            Status::Idle => "Idle",
            Status::Working => "Working",
            Status::Done => "Done",
            Status::NeedsApproval => "Needs approval",
            Status::Question => "Question",
            Status::NeedsSignIn => "Needs sign-in",
            Status::NeedsInput => "Needs input",
            Status::Dialog => "Dialog",
            Status::Error => "Error",
        }
    }

    /// The status's name in JSON output and the audit log. This is its only spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Working => "working",
            Status::Done => "done",
            Status::NeedsApproval => "needs_approval",
            Status::Question => "question",
            Status::NeedsSignIn => "needs_sign_in",
            Status::NeedsInput => "needs_input",
            Status::Dialog => "dialog",
            Status::Error => "error",
        }
    }

    pub fn is_blocked(self) -> bool {
        matches!(
            self,
            Status::NeedsApproval
                | Status::Question
                | Status::NeedsSignIn
                | Status::NeedsInput
                | Status::Dialog
        )
    }

    /// The exit code `send` and `wait` end with when the program stops in this status.
    pub fn exit_code(self) -> i32 {
        match self {
            Status::Idle | Status::Done | Status::Working => exit::OK,
            // A person answers the dialog in Rex, as with a permission request.
            Status::NeedsApproval | Status::Dialog => exit::NEEDS_APPROVAL,
            Status::Question => exit::QUESTION,
            Status::NeedsInput => exit::NEEDS_INPUT,
            Status::NeedsSignIn => exit::NEEDS_SIGN_IN,
            Status::Error => exit::PROGRAM_ERROR,
        }
    }
}

impl Serialize for Status {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A pane's records by id (the root record's id is empty), and their fold.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Records(BTreeMap<String, Record>);

impl Records {
    /// A `program_status` reply's records.
    pub fn from_reply(reply: &Value) -> Self {
        let mut records = Records::default();
        for r in reply["records"].as_array().into_iter().flatten() {
            records.0.insert(
                r["id"].as_str().unwrap_or_default().to_owned(),
                Record::parse(r),
            );
        }
        records
    }

    /// Applies one terminal block event's payload. Returns whether it changed anything.
    pub fn apply(&mut self, event: &BlockEvent) -> bool {
        match event {
            BlockEvent::StatusChanged { id, record } => {
                self.0.insert(id.clone(), record.clone()).as_ref() != Some(record)
            }
            BlockEvent::StatusRemoved { ids } => {
                let mut changed = false;
                for id in ids {
                    changed |= self.0.remove(id).is_some();
                }
                changed
            }
        }
    }

    fn top(&self) -> Option<&Record> {
        // `max_by_key` keeps the last of equal keys; ids sort parents first, so the deepest
        // record with the highest state decides, which carries the most specific message.
        self.0.values().max_by_key(|r| r.state)
    }

    pub fn status(&self) -> Status {
        self.top()
            .map(|r| Status::from_record(r.state, r.kind))
            .unwrap_or_default()
    }

    pub fn msg(&self) -> Option<&str> {
        self.top().and_then(|r| r.msg.as_deref())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn apps(&self) -> impl Iterator<Item = &str> {
        self.0.values().filter_map(|r| r.app.as_deref())
    }
}

/// A terminal `block_event` about program status.
#[derive(Clone, Debug, PartialEq)]
pub enum BlockEvent {
    StatusChanged { id: String, record: Record },
    StatusRemoved { ids: Vec<String> },
}

/// The block and meaning of a `block_event` payload; `None` for anything but status changes.
pub fn parse_block_event(payload: &Value) -> Option<(BlockId, BlockEvent)> {
    if payload["creator_name"] != TERMINAL_CREATOR {
        return None;
    }
    let block = BlockId::from(payload["block_id"].as_str()?);
    let body = &payload["payload"];
    let event = match payload["name"].as_str()? {
        "program_status_changed" => BlockEvent::StatusChanged {
            id: body["id"].as_str().unwrap_or_default().to_owned(),
            record: Record::parse(&body["record"]),
        },
        "program_status_removed" => BlockEvent::StatusRemoved {
            ids: body["ids"]
                .as_array()?
                .iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect(),
        },
        _ => return None,
    };
    Some((block, event))
}

/// How long a pane must stay Idle after Working before that counts as Done. Claude Code clears
/// its records briefly in the middle of a turn, so Idle alone doesn't mean finished.
pub const IDLE_GRACE: Duration = Duration::from_secs(5);

/// Decides when `send` or `wait` is finished, from the statuses seen after it started.
#[derive(Debug, Clone)]
pub struct Tracker {
    current: Status,
    saw_working: bool,
    /// `send` ignores the status it started with: only a change after the message counts.
    need_change: bool,
    /// When the pane went Idle after Working, if it still is.
    idle_since: Option<Instant>,
}

impl Tracker {
    /// For `send`: the program has just been given a message.
    pub fn after_send(initial: Status) -> Self {
        Tracker {
            current: initial,
            saw_working: false,
            need_change: true,
            idle_since: None,
        }
    }

    /// For `wait`: any status but Working is already an answer.
    pub fn waiting(initial: Status) -> Self {
        Tracker {
            current: initial,
            saw_working: initial == Status::Working,
            need_change: false,
            idle_since: None,
        }
    }

    pub fn current(&self) -> Status {
        self.current
    }

    /// The final status, if `wait` should end before any change.
    pub fn settled_now(&self) -> Option<Status> {
        (!self.need_change && self.current != Status::Working).then_some(self.current)
    }

    /// Records a newly folded status from a live report, seen at `now`. Returns the final
    /// status, if this one ends the wait.
    pub fn observe(&mut self, status: Status, now: Instant) -> Option<Status> {
        self.current = status;
        if status != Status::Idle {
            self.idle_since = None;
        }
        match status {
            Status::Working => {
                self.saw_working = true;
                None
            }
            // A program that finishes may clear its records instead of reporting done, but
            // that only counts once the pane has stayed Idle for IDLE_GRACE.
            Status::Idle => {
                if self.saw_working && self.idle_since.is_none() {
                    self.idle_since = Some(now);
                }
                None
            }
            _ => Some(status),
        }
    }

    /// When an Idle that followed Working will count as Done, if one is pending.
    pub fn idle_deadline(&self) -> Option<Instant> {
        self.idle_since.map(|since| since + IDLE_GRACE)
    }

    /// Done, if the pane has stayed Idle after Working for IDLE_GRACE by `now`.
    pub fn settled_by(&self, now: Instant) -> Option<Status> {
        self.idle_deadline()
            .is_some_and(|deadline| now >= deadline)
            .then_some(Status::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(line: &str) -> (BlockId, BlockEvent) {
        parse_block_event(&serde_json::from_str(line).unwrap()).unwrap()
    }

    const BLOCK: &str = "block:01234567-89ab-cdef-0123-456789abcdef";

    #[test]
    fn parses_status_events() {
        let (block, changed) = event(
            r#"{"block_id":"block:01234567-89ab-cdef-0123-456789abcdef","creator_name":"com.superlogical.terminal","name":"program_status_changed","payload":{"id":"build","record":{"id":"build","state":"blocked","kind":"permission","effective_app":"claude","updated_at":"2026-11-02T08:52:57.911Z"},"reason":"report"}}"#,
        );
        assert_eq!(block.as_str(), BLOCK);
        let BlockEvent::StatusChanged { id, record } = &changed else {
            panic!("{changed:?}")
        };
        assert_eq!(id, "build");
        assert_eq!(record.state, State::Blocked);
        assert_eq!(record.kind, Some(BlockedKind::Permission));
        assert_eq!(record.app.as_deref(), Some("claude"));

        let (_, removed) = event(
            r#"{"block_id":"block:x","creator_name":"com.superlogical.terminal","name":"program_status_removed","payload":{"ids":["kid"],"reason":"foreground"}}"#,
        );
        assert_eq!(
            removed,
            BlockEvent::StatusRemoved {
                ids: vec!["kid".into()]
            }
        );
    }

    #[test]
    fn ignores_other_events() {
        for payload in [
            json!({"block_id":"b","creator_name":"other","name":"program_status_changed","payload":{}}),
            json!({"block_id":"b","creator_name":TERMINAL_CREATOR,"name":"title_changed","payload":{}}),
        ] {
            assert!(parse_block_event(&payload).is_none());
        }
    }

    #[test]
    fn folds_highest_record_and_maps_kinds() {
        let mut records = Records::from_reply(&json!({"records":[
            {"state":"working","app":"claude","updated_at":"t"},
            {"id":"t","state":"done","updated_at":"t"}
        ]}));
        assert_eq!(records.status(), Status::Working);
        assert!(records.apps().any(|a| a == "claude"));

        for (kind, status) in [
            (Some("permission"), Status::NeedsApproval),
            (Some("question"), Status::Question),
            (Some("auth"), Status::NeedsSignIn),
            (None, Status::NeedsInput),
            (Some("future-kind"), Status::NeedsInput),
        ] {
            let mut record = json!({"state":"blocked","msg":"Allow Bash?"});
            if let Some(kind) = kind {
                record["kind"] = json!(kind);
            }
            let changed = BlockEvent::StatusChanged {
                id: "x".into(),
                record: Record::parse(&record),
            };
            records.apply(&changed);
            assert_eq!(records.status(), status);
            assert_eq!(records.msg(), Some("Allow Bash?"));
        }
        assert!(records.apply(&BlockEvent::StatusRemoved {
            ids: vec!["x".into()]
        }));
        assert_eq!(records.status(), Status::Working);
        assert!(!records.apply(&BlockEvent::StatusRemoved {
            ids: vec!["x".into()]
        }));
    }

    #[test]
    fn error_outranks_working_and_unknown_is_idle() {
        let records = Records::from_reply(&json!({"records":[
            {"state":"working"},{"id":"a","state":"error"},{"id":"b","state":"later"}
        ]}));
        assert_eq!(records.status(), Status::Error);
        assert_eq!(Records::default().status(), Status::Idle);
    }

    #[test]
    fn exit_codes_are_distinct() {
        let codes: std::collections::HashSet<_> = [
            Status::NeedsApproval,
            Status::Question,
            Status::NeedsInput,
            Status::NeedsSignIn,
            Status::Error,
        ]
        .iter()
        .map(|s| s.exit_code())
        .chain([
            exit::OK,
            exit::TIMEOUT,
            exit::ERROR,
            exit::REFUSED,
            exit::USAGE,
            exit::LOST_CONNECTION,
        ])
        .collect();
        assert_eq!(codes.len(), 11);
        assert_eq!(Status::Done.exit_code(), 0);
    }

    #[test]
    fn serializes_as_its_name() {
        for status in [
            Status::Idle,
            Status::Working,
            Status::Done,
            Status::NeedsApproval,
            Status::Question,
            Status::NeedsSignIn,
            Status::NeedsInput,
            Status::Error,
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), status.as_str());
        }
    }

    #[test]
    fn send_ignores_the_status_it_started_with() {
        let now = Instant::now();
        let mut t = Tracker::after_send(Status::Done);
        assert_eq!(t.settled_now(), None);
        assert_eq!(t.observe(Status::Working, now), None);
        assert_eq!(t.observe(Status::Done, now), Some(Status::Done));

        let mut t = Tracker::after_send(Status::Working);
        assert_eq!(
            t.observe(Status::NeedsApproval, now),
            Some(Status::NeedsApproval)
        );
    }

    #[test]
    fn wait_settles_on_anything_but_working() {
        assert_eq!(
            Tracker::waiting(Status::Question).settled_now(),
            Some(Status::Question)
        );
        assert_eq!(
            Tracker::waiting(Status::Idle).settled_now(),
            Some(Status::Idle)
        );
        let now = Instant::now();
        let mut t = Tracker::waiting(Status::Working);
        assert_eq!(t.settled_now(), None);
        assert_eq!(t.observe(Status::Idle, now), None);
        assert_eq!(t.settled_by(now + IDLE_GRACE), Some(Status::Done));
    }

    #[test]
    fn idle_after_working_is_done_only_once_it_lasts() {
        let start = Instant::now();
        let mut t = Tracker::after_send(Status::Idle);
        assert_eq!(t.observe(Status::Idle, start), None, "before any work");
        assert_eq!(t.idle_deadline(), None);
        assert_eq!(t.observe(Status::Working, start), None);
        // Claude Code clears its records mid-turn, then reports Working again.
        let blip = start + Duration::from_secs(1);
        assert_eq!(t.observe(Status::Idle, blip), None);
        assert_eq!(t.idle_deadline(), Some(blip + IDLE_GRACE));
        assert_eq!(t.settled_by(blip + Duration::from_secs(2)), None);
        assert_eq!(
            t.observe(Status::Working, blip + Duration::from_secs(2)),
            None
        );
        assert_eq!(
            t.settled_by(blip + IDLE_GRACE),
            None,
            "the blip was cancelled"
        );
        // An Idle that lasts is Done; a repeated Idle report doesn't restart the clock.
        let quiet = start + Duration::from_secs(10);
        assert_eq!(t.observe(Status::Idle, quiet), None);
        assert_eq!(
            t.observe(Status::Idle, quiet + Duration::from_secs(3)),
            None
        );
        assert_eq!(t.settled_by(quiet + IDLE_GRACE), Some(Status::Done));
    }
}
