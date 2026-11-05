//! The calls the relay makes on a Rex server's control API.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use rexrs::api::session::{self, ViewResponse};
use rexrs::api::terminal::{self, ProcessResponse};
use rexrs::api::{BlockMethod, Layout};
use rexrs::{ClientKind, Event, RexClient};

use crate::endpoint::Endpoint;
use crate::ids::{BlockId, SessionId};
use crate::status::Records;
use crate::transport::{self, Transport};

pub const MIN_API_REVISION: u32 = 7;
pub const API_REVISION: u32 = 8;
/// Program status (`program_status` and its events) needs revision 8.
pub const STATUS_API_REVISION: u32 = 8;
pub struct Rex {
    client: RexClient<Transport>,
    /// How long connecting and each call may take before the connection counts as stalled.
    call_timeout: Duration,
}

/// A session on the server.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: SessionId,
    pub label: String,
}

/// What `create` made: the session, and the block its program runs in.
#[derive(Debug, Clone)]
pub struct Created {
    pub session: SessionId,
    pub block: Option<BlockId>,
}

impl Rex {
    /// Connects, waiting at most `call_timeout` for each step and each later call. `resume` is
    /// the reconnect token of an earlier connection, to continue its client.
    pub fn connect(
        endpoint: &Endpoint,
        call_timeout: Duration,
        resume: Option<&str>,
    ) -> Result<Self> {
        let transport = transport::connect(endpoint, rexrs::CONTROL_PATH, call_timeout)?;
        let mut builder = RexClient::builder()
            .name("rex-relay")
            .version(env!("CARGO_PKG_VERSION"))
            .kind(ClientKind::Cli)
            .api_revisions(MIN_API_REVISION..=API_REVISION);
        if let Some(token) = resume {
            builder = builder.reconnect_token(token);
        }
        let client = builder
            .connect_with(transport)
            .with_context(|| format!("greeting {endpoint}"))?;
        let revision = client.welcome().api_revision;
        if revision < STATUS_API_REVISION {
            bail!(
                "{endpoint} speaks control API revision {revision}; rexctl needs revision {STATUS_API_REVISION} for program status"
            );
        }
        Ok(Rex {
            client,
            call_timeout,
        })
    }

    /// The token a later connection passes to continue this client. Never log it.
    pub fn reconnect_token(&self) -> Option<String> {
        self.client
            .welcome()
            .client
            .as_ref()
            .map(|c| c.reconnect_token.clone())
    }

    fn set_read_timeout(&mut self, timeout: Duration) -> Result<()> {
        self.client
            .transport()
            .socket()
            .get_ref()
            .set_read_timeout(Some(timeout))
            .context("setting the read timeout")
    }

    pub fn sessions(&mut self) -> Result<Vec<Session>> {
        Ok(self
            .client
            .call(session::List::new())?
            .sessions
            .into_iter()
            .map(|s| Session {
                id: s.session_id.into(),
                label: s.label,
            })
            .collect())
    }

    /// The session with this id, or else the only one with this label.
    pub fn resolve(&mut self, wanted: &str) -> Result<Session> {
        let sessions = self.sessions()?;
        if let Some(s) = sessions.iter().find(|s| s.id.as_str() == wanted) {
            return Ok(s.clone());
        }
        let labelled: Vec<_> = sessions.iter().filter(|s| s.label == wanted).collect();
        match labelled.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("no session with id or label {wanted:?}"),
            _ => bail!("more than one session is labelled {wanted:?}; use its session id"),
        }
    }

    /// Lets this connection call methods on every session without joining any.
    pub fn observe_all(&mut self) -> Result<()> {
        self.client.call(session::AttachAll::new("session"))?;
        Ok(())
    }

    /// Joins a session and starts receiving its events, `block_event` included. Returns the
    /// view and the seq it is current as of.
    pub fn attach(&mut self, session_id: &SessionId) -> Result<(u64, ViewResponse)> {
        let reply = self
            .client
            .call_with_seq(session::Attach::new(session_id.as_str()))?;
        Ok((reply.seq, reply.value.view))
    }

    pub fn view(&mut self, session_id: &SessionId) -> Result<ViewResponse> {
        Ok(self.client.call(session::View::new(session_id.as_str()))?)
    }

    /// The pane's status records, and the seq they are current as of.
    pub fn program_status(
        &mut self,
        session_id: &SessionId,
        block_id: &BlockId,
    ) -> Result<(u64, Records)> {
        let reply = self.client.call_with_seq(
            terminal::ProgramStatus::new().on(session_id.as_str(), block_id.as_str()),
        )?;
        let value = serde_json::to_value(&reply.value)?;
        Ok((reply.seq, Records::from_reply(&value)))
    }

    pub fn process(
        &mut self,
        session_id: &SessionId,
        block_id: &BlockId,
    ) -> Result<ProcessResponse> {
        Ok(self
            .client
            .call(terminal::Process::new().on(session_id.as_str(), block_id.as_str()))?)
    }

    pub fn title(&mut self, session_id: &SessionId, block_id: &BlockId) -> Result<String> {
        Ok(self
            .client
            .call(terminal::Title::new().on(session_id.as_str(), block_id.as_str()))?
            .title)
    }

    /// Whether the program in the pane turned on bracketed paste; `None` if the server didn't
    /// say.
    pub fn bracketed_paste(
        &mut self,
        session_id: &SessionId,
        block_id: &BlockId,
    ) -> Result<Option<bool>> {
        let modes = self
            .client
            .call(terminal::Modes::new().on(session_id.as_str(), block_id.as_str()))?;
        Ok(modes
            .modes
            .iter()
            .find(|m| m.name == "bracketed_paste" || (m.number == 2004 && m.ansi != Some(true)))
            .map(|m| m.set))
    }

    /// The pane's screen and scrollback as plain text.
    pub fn read_text(&mut self, session_id: &SessionId, block_id: &BlockId) -> Result<String> {
        const CHUNK: i64 = 512 * 1024;
        let first = self.client.call(
            terminal::Format::new()
                .format("text")
                .trim(true)
                .unwrap(true)
                .max_bytes(CHUNK)
                .on(session_id.as_str(), block_id.as_str()),
        )?;
        let mut text = first.content;
        let Some(id) = first.id else {
            return Ok(text);
        };
        loop {
            let part = self.client.call(
                terminal::BlobRead::new()
                    .id(id)
                    .offset(text.len() as i64)
                    .max_bytes(CHUNK)
                    .on(session_id.as_str(), block_id.as_str()),
            )?;
            if part.data.is_empty() && !part.eof {
                bail!("the server returned no text before the end of the pane's content");
            }
            text.push_str(&part.data);
            if part.eof {
                return Ok(text);
            }
        }
    }

    /// Types `data` into the pane.
    pub fn write(&mut self, session_id: &SessionId, block_id: &BlockId, data: &str) -> Result<()> {
        self.client
            .call(terminal::Write::new(data).on(session_id.as_str(), block_id.as_str()))?;
        Ok(())
    }

    /// Creates a session with one window whose one terminal runs `command` in `cwd`. Without a
    /// label, the server picks one.
    pub fn create(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        command: &[String],
    ) -> Result<Created> {
        let mut layout = Layout::shell().label("claude");
        if let Some(cwd) = cwd {
            layout = layout.cwd(cwd);
        }
        if !command.is_empty() {
            layout = layout.command(command.iter().cloned());
        }
        let window = session::CreateInitialWindow::new()
            .window_label("claude")
            .layout(layout);
        let mut request = session::Create::new().initial_windows([window]);
        if let Some(label) = label {
            request = request.label(label);
        }
        let created = self.client.call(request)?;
        let block = created
            .initial_windows
            .into_iter()
            .next()
            .and_then(|w| w.block_ids.into_iter().next())
            .map(BlockId::from);
        Ok(Created {
            session: created.session_id.into(),
            block,
        })
    }

    /// Closes a session and every block in it. Callers must check ownership first.
    pub fn destroy(&mut self, session_id: &SessionId) -> Result<()> {
        self.client
            .call(session::Destroy::new(session_id.as_str()))?;
        Ok(())
    }

    /// The next event, or `None` if none arrives within `wait`.
    pub fn next_event(&mut self, wait: Duration) -> Result<Option<Event>> {
        self.set_read_timeout(wait)?;
        let result = self.client.next_event();
        self.set_read_timeout(self.call_timeout)?;
        match result {
            Ok(event) => Ok(Some(event)),
            Err(rexrs::Error::TimedOut) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Ends the connection with a close handshake. Don't call this on a connection that has
    /// stalled: the handshake waits for the server. Drop the `Rex` instead.
    pub fn close(self) {
        let _ = self.client.close();
    }
}

/// The WebSocket close code a server sends when it refuses the client, such as for sharing no
/// API revision with it. Retrying can't change the answer.
const POLICY_VIOLATION: u16 = 1008;

/// Whether `error` means the connection couldn't be made, stalled, or broke, as opposed to the
/// server answering with an error. Callers retry after the first kind and stop on the second.
pub fn is_connection_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let retryable = match cause.downcast_ref::<rexrs::Error>() {
            Some(rexrs::Error::Closed { code, .. }) => *code != Some(POLICY_VIOLATION),
            Some(rexrs::Error::TimedOut | rexrs::Error::Transport(_)) => true,
            _ => false,
        };
        retryable || cause.downcast_ref::<std::io::Error>().is_some()
    })
}

/// The pause after the `failures`th failed connection attempt in a row: 1s, 2s, 4s, then 8s.
pub fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1 << failures.saturating_sub(1).min(3))
}

/// The server couldn't be reached within the time allowed. Exits with
/// [`crate::exit::LOST_CONNECTION`].
#[derive(Debug)]
pub struct Unreachable {
    pub endpoint: String,
    pub attempts: u32,
    pub waited: Duration,
    /// The last attempt's error.
    pub last: String,
}

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "couldn't connect to {} in {} attempts over {}s: {}",
            self.endpoint,
            self.attempts,
            self.waited.as_secs(),
            self.last
        )
    }
}

impl std::error::Error for Unreachable {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_8s() {
        let pauses: Vec<u64> = (1..=6).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(pauses, [1, 2, 4, 8, 8, 8]);
    }

    #[test]
    fn refusals_by_policy_are_not_retried() {
        let refused = anyhow::Error::new(rexrs::Error::Closed {
            code: Some(POLICY_VIOLATION),
            reason: "no common API revision".into(),
        });
        assert!(!is_connection_error(&refused));
        let dropped = anyhow::Error::new(rexrs::Error::Closed {
            code: None,
            reason: String::new(),
        });
        assert!(is_connection_error(&dropped));
        assert!(is_connection_error(&anyhow::Error::new(
            rexrs::Error::TimedOut
        )));
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(is_connection_error(
            &anyhow::Error::new(io).context("connecting")
        ));
        assert!(!is_connection_error(&anyhow::anyhow!("speaks revision 7")));
    }
}
