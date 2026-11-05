//! The calls the relay makes on a Rex server's control API.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use rexrs::api::session::{self, Session, ViewResponse};
use rexrs::api::terminal::{self, ProcessResponse};
use rexrs::api::{BlockMethod, Layout};
use rexrs::{ClientKind, Event, RexClient};

use crate::endpoint::Endpoint;
use crate::status::Records;
use crate::transport::{self, Transport};

pub const MIN_API_REVISION: u32 = 7;
pub const API_REVISION: u32 = 8;
/// Program status (`program_status` and its events) needs revision 8.
pub const STATUS_API_REVISION: u32 = 8;
/// How long connecting and each call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Rex {
    client: RexClient<Transport>,
}

impl Rex {
    pub fn connect(endpoint: &Endpoint) -> Result<Self> {
        let transport = transport::connect(endpoint, rexrs::CONTROL_PATH, CALL_TIMEOUT)?;
        let client = RexClient::builder()
            .name("rex-relay")
            .version(env!("CARGO_PKG_VERSION"))
            .kind(ClientKind::Cli)
            .api_revisions(MIN_API_REVISION..=API_REVISION)
            .connect_with(transport)
            .with_context(|| format!("greeting {endpoint}"))?;
        let revision = client.welcome().api_revision;
        if revision < STATUS_API_REVISION {
            bail!(
                "{endpoint} speaks control API revision {revision}; rexctl needs revision {STATUS_API_REVISION} for program status"
            );
        }
        Ok(Rex { client })
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
        Ok(self.client.call(session::List::new())?.sessions)
    }

    /// The session with this id, or else the only one with this label.
    pub fn resolve(&mut self, wanted: &str) -> Result<Session> {
        let sessions = self.sessions()?;
        if let Some(s) = sessions.iter().find(|s| s.session_id == wanted) {
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
    pub fn attach(&mut self, session_id: &str) -> Result<(u64, ViewResponse)> {
        let reply = self
            .client
            .call_with_seq(session::Attach::new(session_id))?;
        Ok((reply.seq, reply.value.view))
    }

    pub fn view(&mut self, session_id: &str) -> Result<ViewResponse> {
        Ok(self.client.call(session::View::new(session_id))?)
    }

    /// The pane's status records, and the seq they are current as of.
    pub fn program_status(&mut self, session_id: &str, block_id: &str) -> Result<(u64, Records)> {
        let reply = self
            .client
            .call_with_seq(terminal::ProgramStatus::new().on(session_id, block_id))?;
        let value = serde_json::to_value(&reply.value)?;
        Ok((reply.seq, Records::from_reply(&value)))
    }

    pub fn process(&mut self, session_id: &str, block_id: &str) -> Result<ProcessResponse> {
        Ok(self
            .client
            .call(terminal::Process::new().on(session_id, block_id))?)
    }

    pub fn title(&mut self, session_id: &str, block_id: &str) -> Result<String> {
        Ok(self
            .client
            .call(terminal::Title::new().on(session_id, block_id))?
            .title)
    }

    /// Whether the program in the pane turned on bracketed paste; `None` if the server didn't
    /// say.
    pub fn bracketed_paste(&mut self, session_id: &str, block_id: &str) -> Result<Option<bool>> {
        let modes = self
            .client
            .call(terminal::Modes::new().on(session_id, block_id))?;
        Ok(modes
            .modes
            .iter()
            .find(|m| m.name == "bracketed_paste" || (m.number == 2004 && m.ansi != Some(true)))
            .map(|m| m.set))
    }

    /// The pane's screen and scrollback as plain text.
    pub fn read_text(&mut self, session_id: &str, block_id: &str) -> Result<String> {
        const CHUNK: i64 = 512 * 1024;
        let first = self.client.call(
            terminal::Format::new()
                .format("text")
                .trim(true)
                .unwrap(true)
                .max_bytes(CHUNK)
                .on(session_id, block_id),
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
                    .on(session_id, block_id),
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
    pub fn write(&mut self, session_id: &str, block_id: &str, data: &str) -> Result<()> {
        self.client
            .call(terminal::Write::new(data).on(session_id, block_id))?;
        Ok(())
    }

    /// Creates a session with one window whose one terminal runs `command` in `cwd`.
    pub fn create(
        &mut self,
        label: &str,
        cwd: Option<&str>,
        command: &[String],
    ) -> Result<session::CreateResponse> {
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
        Ok(self.client.call(
            session::Create::new()
                .label(label)
                .initial_windows([window]),
        )?)
    }

    /// Closes a session and every block in it. Callers must check ownership first.
    pub fn destroy(&mut self, session_id: &str) -> Result<()> {
        self.client.call(session::Destroy::new(session_id))?;
        Ok(())
    }

    /// The next event, or `None` if none arrives within `wait`.
    pub fn next_event(&mut self, wait: Duration) -> Result<Option<Event>> {
        self.set_read_timeout(wait)?;
        let result = self.client.next_event();
        self.set_read_timeout(CALL_TIMEOUT)?;
        match result {
            Ok(event) => Ok(Some(event)),
            Err(rexrs::Error::TimedOut) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn close(self) {
        let _ = self.client.close();
    }
}
