//! The append-only audit log of mutating calls, one JSON object per line.
//!
//! Entries hold what was done and where, never what was said: a message appears only as its
//! SHA-256 and length. Endpoints are logged in canonical form, which carries no credentials.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, Serialize)]
pub struct Entry {
    pub timestamp: String,
    /// `new`, `send`, or `close`.
    pub command: &'static str,
    pub endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_len: Option<usize>,
    /// Such as `created`, `sent`, `done`, `needs_approval`, `timeout`, `closed`, `refused: …`,
    /// `error: …`.
    pub result: String,
}

/// The current time in RFC 3339, UTC, to the millisecond.
pub fn now() -> String {
    humantime::format_rfc3339_millis(SystemTime::now()).to_string()
}

impl Entry {
    pub fn new(command: &'static str, endpoint: impl Into<String>) -> Self {
        Entry {
            timestamp: now(),
            command,
            endpoint: endpoint.into(),
            ..Entry::default()
        }
    }

    pub fn message(mut self, message: &str) -> Self {
        self.msg_sha256 = Some(sha256_hex(message.as_bytes()));
        self.msg_len = Some(message.len());
        self
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        AuditLog { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Checks that entries can be appended, before doing anything that needs one.
    pub fn ensure_writable(&self) -> Result<()> {
        self.open().map(drop)
    }

    /// Appends one entry. The file and its directory are private to the user.
    pub fn append(&self, entry: &Entry) -> Result<()> {
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        let mut file = self.open()?;
        // One write call per line, so concurrent relays don't interleave within a line.
        file.write_all(line.as_bytes())
            .with_context(|| format!("writing audit log {}", self.path.display()))
    }

    fn open(&self) -> Result<fs::File> {
        if let Some(dir) = self.path.parent() {
            create_private_dir(dir)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&self.path)
            .with_context(|| format!("opening audit log {}", self.path.display()))
    }
}

fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_messages() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn appends_lines_without_message_text() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(dir.path().join("state/rex-relay/audit.jsonl"));
        let mut first =
            Entry::new("send", "https://rex-host.example.ts.net").message("secret plan");
        first.session = Some("session:1".into());
        first.block = Some("block:1".into());
        first.result = "sent".into();
        log.append(&first).unwrap();
        let mut second = Entry::new("new", "https://rex-host.example.ts.net");
        second.result = "created".into();
        log.append(&second).unwrap();

        let text = fs::read_to_string(log.path()).unwrap();
        assert!(!text.contains("secret plan"));
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["msg_len"], 11);
        assert_eq!(lines[0]["msg_sha256"], sha256_hex(b"secret plan"));
        assert_eq!(lines[0]["block"], "block:1");
        assert_eq!(lines[1]["command"], "new");
        assert!(lines[1].get("msg_sha256").is_none());
        assert!(lines[0]["timestamp"].as_str().unwrap().ends_with('Z'));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(log.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
