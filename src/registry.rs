//! The sessions `rex-relay new` created: `~/.local/state/rex-relay/owned.json`.
//!
//! Ownership is by session id and endpoint, never by label: Claude Code renames the sessions it
//! runs in, so a label proves nothing. `send` targets only owned sessions (unless told
//! otherwise) and `close` closes only owned sessions, always.
//!
//! Every change happens under an exclusive lock on `owned.json.lock` and is written to a
//! temporary file that is then renamed over `owned.json`, so readers never see a partial file.
//! A file that doesn't parse is an error, never an empty registry: the registry fails closed.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::ids::SessionId;

const VERSION: u32 = 1;

/// One session the relay created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owned {
    /// The canonical endpoint the session lives on.
    pub endpoint: String,
    pub session_id: SessionId,
    /// The label it was created with, if `new` was given one. Its current label may differ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Contents {
    version: u32,
    #[serde(default)]
    sessions: Vec<Owned>,
}

#[derive(Debug, Clone)]
pub struct Registry {
    path: PathBuf,
}

impl Registry {
    pub fn new(path: PathBuf) -> Self {
        Registry { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every owned session, on every endpoint. A missing file is an empty registry.
    pub fn load(&self) -> Result<Vec<Owned>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        let contents: Contents = serde_json::from_str(&text)
            .with_context(|| format!("parsing the ownership registry {}", self.path.display()))?;
        if contents.version != VERSION {
            bail!(
                "{} has version {}; this rex-relay reads version {VERSION}",
                self.path.display(),
                contents.version
            );
        }
        Ok(contents.sessions)
    }

    /// The owned sessions on `endpoint`.
    pub fn on(&self, endpoint: &str) -> Result<Vec<Owned>> {
        let mut sessions = self.load()?;
        sessions.retain(|s| s.endpoint == endpoint);
        Ok(sessions)
    }

    pub fn find(&self, endpoint: &str, session_id: &SessionId) -> Result<Option<Owned>> {
        Ok(self
            .on(endpoint)?
            .into_iter()
            .find(|s| &s.session_id == session_id))
    }

    /// Checks that the registry can be read and changed, before doing anything that would
    /// need to change it.
    pub fn ensure_writable(&self) -> Result<()> {
        let _lock = self.lock()?;
        self.load().map(drop)
    }

    pub fn add(&self, owned: Owned) -> Result<()> {
        self.update(|sessions| {
            sessions
                .retain(|s| !(s.endpoint == owned.endpoint && s.session_id == owned.session_id));
            sessions.push(owned);
        })
        .map(drop)
    }

    /// Removes a session. Returns whether it was there.
    pub fn remove(&self, endpoint: &str, session_id: &SessionId) -> Result<bool> {
        self.update(|sessions| {
            let before = sessions.len();
            sessions.retain(|s| !(s.endpoint == endpoint && &s.session_id == session_id));
            sessions.len() != before
        })
    }

    fn update<T>(&self, change: impl FnOnce(&mut Vec<Owned>) -> T) -> Result<T> {
        let _lock = self.lock()?;
        let mut sessions = self.load()?;
        let result = change(&mut sessions);
        self.save(sessions)?;
        Ok(result)
    }

    /// An exclusive lock, held until the file is dropped.
    fn lock(&self) -> Result<File> {
        let dir = self
            .path
            .parent()
            .context("registry path has no directory")?;
        create_private_dir(dir)?;
        let mut lock_path = self.path.clone().into_os_string();
        lock_path.push(".lock");
        let file = private_options()
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", Path::new(&lock_path).display()))?;
        file.lock()
            .with_context(|| format!("locking {}", Path::new(&lock_path).display()))?;
        Ok(file)
    }

    fn save(&self, sessions: Vec<Owned>) -> Result<()> {
        let contents = Contents {
            version: VERSION,
            sessions,
        };
        let mut text = serde_json::to_string_pretty(&contents)?;
        text.push('\n');
        let dir = self
            .path
            .parent()
            .context("registry path has no directory")?;
        let name = self
            .path
            .file_name()
            .context("registry path has no file name")?
            .to_string_lossy();
        let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
        let write = || -> Result<()> {
            let mut file = private_options()
                .write(true)
                .truncate(true)
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, &self.path)
                .with_context(|| format!("replacing {}", self.path.display()))?;
            // Make the rename itself durable.
            if let Ok(dir) = File::open(dir) {
                let _ = dir.sync_all();
            }
            Ok(())
        };
        let result = write();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
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

    fn owned(endpoint: &str, id: &str) -> Owned {
        Owned {
            endpoint: endpoint.into(),
            session_id: id.into(),
            label: Some("fix-ci".into()),
            created_at: "2026-11-04T00:00:00.000Z".into(),
            purpose: Some("fix CI".into()),
        }
    }

    const A: &str = "https://rex-host.example.ts.net";
    const B: &str = "https://other.example.ts.net";

    #[test]
    fn adds_finds_and_removes_per_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path().join("state/owned.json"));
        assert!(registry.load().unwrap().is_empty());
        registry.ensure_writable().unwrap();

        registry.add(owned(A, "session:1")).unwrap();
        registry.add(owned(B, "session:1")).unwrap();
        registry.add(owned(A, "session:2")).unwrap();
        registry.add(owned(A, "session:2")).unwrap();
        assert_eq!(registry.load().unwrap().len(), 3);
        assert_eq!(registry.on(A).unwrap().len(), 2);
        assert_eq!(
            registry
                .find(A, &"session:1".into())
                .unwrap()
                .unwrap()
                .purpose
                .as_deref(),
            Some("fix CI")
        );
        assert!(registry.find(A, &"session:9".into()).unwrap().is_none());

        assert!(registry.remove(A, &"session:1".into()).unwrap());
        assert!(!registry.remove(A, &"session:1".into()).unwrap());
        assert!(registry.find(A, &"session:1".into()).unwrap().is_none());
        assert!(
            registry.find(B, &"session:1".into()).unwrap().is_some(),
            "other endpoint kept"
        );

        let leftovers: Vec<_> = fs::read_dir(registry.path().parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(registry.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn corrupt_or_newer_files_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path().join("owned.json"));
        fs::write(registry.path(), "{not json").unwrap();
        assert!(registry.load().is_err());
        assert!(registry.find(A, &"session:1".into()).is_err());
        assert!(registry.add(owned(A, "session:1")).is_err());
        assert_eq!(fs::read_to_string(registry.path()).unwrap(), "{not json");

        fs::write(registry.path(), r#"{"version":2,"sessions":[]}"#).unwrap();
        assert!(registry.load().is_err());
    }

    #[test]
    fn concurrent_adds_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owned.json");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let registry = Registry::new(path.clone());
                std::thread::spawn(move || registry.add(owned(A, &format!("session:{i}"))).unwrap())
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(Registry::new(path).load().unwrap().len(), 8);
    }
}
