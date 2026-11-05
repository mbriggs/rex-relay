//! Guard rails that don't need the server: message content, labels, and endpoints.

use std::fmt;

use crate::endpoint::{self, Endpoint};
use crate::prompt::Screen;
use crate::status::Status;

/// A request a guard rail refused. Exits with [`crate::exit::REFUSED`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "refused: {}", self.0)
    }
}

impl std::error::Error for Refusal {}

pub fn refuse<T>(reason: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal(reason.into()))
}

/// A message may hold printable text, newlines and tabs, and nothing else: no escape
/// sequences, carriage returns, or other control characters that could act as keys.
pub fn check_message(message: &str, max_bytes: usize) -> Result<(), Refusal> {
    if message.trim().is_empty() {
        return refuse("message is empty");
    }
    if message.len() > max_bytes {
        return refuse(format!(
            "message is {} bytes; the limit is {max_bytes}",
            message.len()
        ));
    }
    if let Some((index, c)) = message
        .char_indices()
        .find(|&(_, c)| c.is_control() && c != '\n' && c != '\t')
    {
        return refuse(format!(
            "message contains control character U+{:04X} at byte {index}; only newline and tab are allowed",
            c as u32
        ));
    }
    Ok(())
}

/// A command for `run`: what `send` accepts, on one line. A newline would run the part before
/// it, and a tab would ask the shell to complete a word.
pub fn check_command(command: &str, max_bytes: usize) -> Result<(), Refusal> {
    check_message(command, max_bytes)?;
    if command.contains(['\n', '\t']) {
        return refuse("command contains a newline or tab; run takes one line");
    }
    Ok(())
}

/// Labels for new sessions: plain text, within Rex's 128-character limit.
pub fn check_label(label: &str) -> Result<(), Refusal> {
    if label.trim().is_empty() {
        return refuse("label is empty");
    }
    if label.chars().count() > 128 {
        return refuse("label is longer than 128 characters");
    }
    if label.chars().any(char::is_control) {
        return refuse("label contains control characters");
    }
    Ok(())
}

/// `--purpose`: one line of plain text.
pub fn check_purpose(purpose: &str) -> Result<(), Refusal> {
    if purpose.trim().is_empty() {
        return refuse("purpose is empty");
    }
    if purpose.chars().count() > 256 {
        return refuse("purpose is longer than 256 characters");
    }
    if purpose.chars().any(char::is_control) {
        return refuse("purpose contains control characters");
    }
    Ok(())
}

/// `send` only targets sessions in the ownership registry, unless the caller opts out.
/// `owned` is whether the session's id is registered for this endpoint.
pub fn check_send_owned(
    owned: bool,
    session: &str,
    label: &str,
    allow_foreign: bool,
) -> Result<(), Refusal> {
    if owned || allow_foreign {
        return Ok(());
    }
    refuse(format!(
        "session {label:?} ({session}) wasn't created by rexctl on this endpoint; pass --allow-foreign to use it anyway"
    ))
}

/// `close` only closes sessions in the ownership registry. There is no override.
pub fn check_close_owned(owned: bool, session: &str, label: &str) -> Result<(), Refusal> {
    if owned {
        return Ok(());
    }
    refuse(format!(
        "session {label:?} ({session}) wasn't created by rexctl on this endpoint; rexctl never closes foreign sessions"
    ))
}

/// The relay never answers prompts on its own. While the program waits for an approval or a
/// question, a message would be taken as the answer, so the caller must say that it is one.
/// Credential prompts are never typed into.
pub fn check_status_for_send(status: Status, answer: bool) -> Result<(), Refusal> {
    match status {
        Status::NeedsSignIn => refuse(
            "the program is waiting for a sign-in; rexctl never types into credential prompts",
        ),
        Status::NeedsApproval | Status::Question if !answer => refuse(format!(
            "the program is in {:?}; pass --answer if this message is meant as the answer",
            status.label()
        )),
        _ => Ok(()),
    }
}

/// `send` types only at Claude Code's input prompt. A menu takes Enter as a choice, so a
/// message sent to one could pick an option, such as "No, exit" in the folder-trust dialog.
pub fn check_screen(screen: Screen) -> Result<(), Refusal> {
    match screen {
        Screen::Prompt => Ok(()),
        Screen::Menu => refuse(
            "the pane shows a menu (such as the folder-trust or a permission dialog), not Claude Code's input prompt; rexctl never picks menu options",
        ),
        Screen::Unknown => refuse(
            "the pane isn't at Claude Code's input prompt; use --block if this is the wrong pane",
        ),
    }
}

/// Unsent text in Claude Code's input box would join the message. Only its length is reported,
/// because the refusal goes to the audit log.
pub fn leftover_input(input: &str) -> Refusal {
    Refusal(format!(
        "Claude Code's input box already holds {} characters of unsent text; pass --clear-input to clear it first",
        input.chars().count()
    ))
}

pub fn check_endpoint(endpoint: &Endpoint, allowlist: &[String]) -> Result<(), Refusal> {
    if endpoint::is_allowed(endpoint, allowlist) {
        return Ok(());
    }
    if allowlist.is_empty() {
        return refuse(format!(
            "endpoint {endpoint} is not allowed: the config's allowed_endpoints is empty"
        ));
    }
    refuse(format!(
        "endpoint {endpoint} is not in the config's allowed_endpoints"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_allow_text_newlines_and_tabs() {
        check_message("fix the bug\n\tthen run tests ✓", 1024).unwrap();
    }

    #[test]
    fn messages_reject_controls_and_oversize() {
        for bad in [
            "",
            "  \n",
            "hi\x1b[201~\r",
            "hi\r",
            "a\x03",
            "a\x7f",
            "a\u{9b}",
            "a\0",
        ] {
            assert!(check_message(bad, 1024).is_err(), "{bad:?}");
        }
        assert!(check_message(&"x".repeat(33), 32).is_err());
        check_message(&"x".repeat(32), 32).unwrap();
    }

    #[test]
    fn commands_are_one_line() {
        check_command("cargo test -- --nocapture", 1024).unwrap();
        for bad in ["ls\nrm -rf x", "ls\t", "ls\r", "\x03", ""] {
            assert!(check_command(bad, 1024).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn labels_are_plain_text() {
        check_label("fix-ci").unwrap();
        assert!(check_label("").is_err());
        assert!(check_label("a\nb").is_err());
        assert!(check_label(&"x".repeat(129)).is_err());
    }

    #[test]
    fn ownership_is_by_registry_not_label() {
        check_send_owned(true, "session:1", "renamed by claude", false).unwrap();
        assert!(check_send_owned(false, "session:2", "lookalike", false).is_err());
        check_send_owned(false, "session:2", "work", true).unwrap();
        check_close_owned(true, "session:1", "anything").unwrap();
        assert!(check_close_owned(false, "session:2", "lookalike").is_err());
    }

    #[test]
    fn purposes_are_one_line() {
        check_purpose("fix the flaky CI job").unwrap();
        for bad in ["", " ", "a\nb", "a\x1b"] {
            assert!(check_purpose(bad).is_err(), "{bad:?}");
        }
        assert!(check_purpose(&"x".repeat(257)).is_err());
    }

    #[test]
    fn send_waits_for_explicit_answers() {
        for status in [Status::NeedsApproval, Status::Question] {
            assert!(check_status_for_send(status, false).is_err());
            check_status_for_send(status, true).unwrap();
        }
        assert!(check_status_for_send(Status::NeedsSignIn, true).is_err());
        for status in [
            Status::Idle,
            Status::Done,
            Status::Working,
            Status::Error,
            Status::NeedsInput,
        ] {
            check_status_for_send(status, false).unwrap();
        }
    }

    #[test]
    fn send_needs_the_input_prompt() {
        check_screen(Screen::Prompt).unwrap();
        assert!(check_screen(Screen::Menu).is_err());
        assert!(check_screen(Screen::Unknown).is_err());
    }

    #[test]
    fn endpoints_need_the_allowlist() {
        let endpoint = Endpoint::parse("https://rex-host.example.ts.net").unwrap();
        assert!(check_endpoint(&endpoint, &[]).is_err());
        assert!(check_endpoint(&endpoint, &["https://other.example.ts.net".into()]).is_err());
        check_endpoint(&endpoint, &["wss://rex-host.example.ts.net/".into()]).unwrap();
    }
}
