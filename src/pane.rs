//! Finding the terminal blocks of a session, and the one that runs Claude Code.

use anyhow::{Result, bail};
use rexrs::api::session::ViewResponse;
use rexrs::api::terminal::ProcessResponse;
use serde::Serialize;

use crate::ids::BlockId;
use crate::status::{Records, TERMINAL_CREATOR};

/// A terminal block in a session's view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pane {
    pub block_id: BlockId,
    pub window_id: Option<String>,
    pub window_label: Option<String>,
    pub label: Option<String>,
    /// The focused block of the session's active window.
    pub focused: bool,
}

/// Every terminal block in the view, in window and layout order.
pub fn panes(view: &ViewResponse) -> Vec<Pane> {
    let mut panes = Vec::new();
    for window in &view.windows {
        let active = window.window_id.is_some() && window.window_id == view.active_window_id;
        for layer in &window.layers {
            for block in &layer.blocks {
                let Some(block_id) = block.block_id.as_deref() else {
                    continue;
                };
                let terminal = block.creator_name.as_deref() == Some(TERMINAL_CREATOR)
                    || block
                        .flavor
                        .as_deref()
                        .is_some_and(|f| f.starts_with(TERMINAL_CREATOR));
                if !terminal {
                    continue;
                }
                let focused = active && window.focused_block_id.as_deref() == Some(block_id);
                panes.push(Pane {
                    block_id: block_id.into(),
                    window_id: window.window_id.clone(),
                    window_label: window.label.clone(),
                    label: block.label.clone(),
                    focused,
                });
            }
        }
    }
    panes
}

/// Whether a program name or path names Claude Code: the native `claude` binary, or the npm
/// package's script (`…/@anthropic-ai/claude-code/cli.js`).
pub fn names_claude(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    let base = base.trim_start_matches('-');
    base == "claude"
        || base == "claude-code"
        || base == "claude code"
        || lower.contains("/claude-code/")
        || lower.contains("/.claude/local/")
}

/// What a pane says about its program.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub records: Records,
    pub process: Option<ProcessResponse>,
    pub title: Option<String>,
}

impl Evidence {
    /// Why this pane looks like Claude Code, if it does.
    pub fn claude_reason(&self) -> Option<&'static str> {
        if self.records.apps().any(names_claude) {
            return Some("program status");
        }
        if let Some(process) = &self.process {
            let foreground = process.foreground.iter().flat_map(|p| {
                [
                    p.script.as_deref(),
                    p.argv0.as_deref(),
                    p.path.as_deref(),
                    Some(p.name.as_str()),
                ]
            });
            let ancestors = process.ancestors.iter().flat_map(|p| {
                [
                    p.script.as_deref(),
                    p.argv0.as_deref(),
                    p.path.as_deref(),
                    Some(p.name.as_str()),
                ]
            });
            if foreground.chain(ancestors).flatten().any(names_claude) {
                return Some("foreground process");
            }
        }
        if self
            .title
            .as_deref()
            .is_some_and(|t| t.to_ascii_lowercase().contains("claude code"))
        {
            return Some("terminal title");
        }
        None
    }

    /// A short name for the foreground program, for listings.
    pub fn program(&self) -> Option<String> {
        if let Some(app) = self.records.apps().next() {
            return Some(app.to_owned());
        }
        let fg = self.process.as_ref()?.foreground.as_ref()?;
        [
            fg.script.as_deref(),
            fg.argv0.as_deref(),
            Some(fg.name.as_str()),
        ]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .map(|s| s.rsplit('/').next().unwrap_or(s).trim_start_matches('-'))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    }
}

/// Matches `--block` against a pane: the full id, the id without `block:`, or a unique prefix.
pub fn find_block<'a>(panes: &'a [Pane], wanted: &str) -> Result<&'a Pane> {
    let wanted = wanted.trim();
    let bare = wanted.strip_prefix("block:").unwrap_or(wanted);
    if bare.is_empty() {
        bail!("--block is empty");
    }
    if let Some(pane) = panes.iter().find(|p| {
        let id = p.block_id.as_str();
        id == wanted || id.strip_prefix("block:") == Some(bare)
    }) {
        return Ok(pane);
    }
    let matches: Vec<_> = panes
        .iter()
        .filter(|p| {
            let id = p.block_id.as_str();
            id.strip_prefix("block:").unwrap_or(id).starts_with(bare)
        })
        .collect();
    match matches.as_slice() {
        [pane] => Ok(pane),
        [] => bail!("no terminal block {wanted:?} in this session"),
        _ => bail!("--block {wanted:?} matches more than one block"),
    }
}

/// Picks the Claude Code pane among `candidates` (each with its evidence): the only one, or
/// else the focused one.
pub fn choose_claude(candidates: &[(Pane, Evidence)]) -> Result<&(Pane, Evidence)> {
    let claude: Vec<_> = candidates
        .iter()
        .filter(|(_, e)| e.claude_reason().is_some())
        .collect();
    match claude.as_slice() {
        [one] => Ok(one),
        [] => bail!(
            "no pane in this session runs Claude Code{}; pass --block to choose one",
            describe(candidates.iter().map(|(p, _)| p))
        ),
        many => {
            if let Some(focused) = many.iter().find(|(p, _)| p.focused) {
                return Ok(focused);
            }
            bail!(
                "more than one pane runs Claude Code{}; pass --block to choose one",
                describe(many.iter().map(|(p, _)| p))
            )
        }
    }
}

fn describe<'a>(panes: impl Iterator<Item = &'a Pane>) -> String {
    let ids: Vec<_> = panes.map(|p| p.block_id.as_str()).collect();
    if ids.is_empty() {
        String::new()
    } else {
        format!(" (terminal blocks: {})", ids.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rexrs::api::terminal::Child;
    use serde_json::json;

    fn view() -> ViewResponse {
        serde_json::from_value(json!({
            "revision": 3,
            "session_id": "session:1",
            "active_window_id": "window:2",
            "windows": [
                {"window_id": "window:1", "focused_block_id": "block:aaa", "layers": [
                    {"kind": "tiled", "blocks": [
                        {"block_id": "block:aaa", "creator_name": TERMINAL_CREATOR},
                        {"block_id": "block:web", "creator_name": "com.example.browser"}
                    ]}
                ]},
                {"window_id": "window:2", "focused_block_id": "block:abb", "layers": [
                    {"kind": "tiled", "blocks": [
                        {"block_id": "block:abb", "flavor": "com.superlogical.terminal.shell", "label": "claude"}
                    ]}
                ]}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn lists_terminal_blocks_and_focus() {
        let panes = panes(&view());
        let ids: Vec<_> = panes.iter().map(|p| p.block_id.as_str()).collect();
        assert_eq!(ids, ["block:aaa", "block:abb"]);
        assert!(!panes[0].focused, "focused in an inactive window");
        assert!(panes[1].focused);
        assert_eq!(panes[1].label.as_deref(), Some("claude"));
    }

    #[test]
    fn finds_blocks_by_id_or_prefix() {
        let panes = panes(&view());
        assert_eq!(
            find_block(&panes, "block:aaa").unwrap().block_id.as_str(),
            "block:aaa"
        );
        assert_eq!(
            find_block(&panes, "abb").unwrap().block_id.as_str(),
            "block:abb"
        );
        assert!(find_block(&panes, "a").is_err(), "ambiguous");
        assert!(find_block(&panes, "zzz").is_err());
        assert!(find_block(&panes, "block:").is_err());
    }

    #[test]
    fn recognizes_claude_names() {
        for name in [
            "claude",
            "/usr/local/bin/claude",
            "/home/u/.npm/lib/node_modules/@anthropic-ai/claude-code/cli.js",
            "/home/u/.claude/local/node_modules/.bin/x",
            "Claude",
        ] {
            assert!(names_claude(name), "{name}");
        }
        for name in ["bash", "claudette", "node", "/opt/claude-tools/run"] {
            assert!(!names_claude(name), "{name}");
        }
    }

    fn process(name: &str, script: Option<&str>) -> ProcessResponse {
        ProcessResponse {
            foreground: Some(Child {
                name: name.into(),
                script: script.map(Into::into),
                ..Child::default()
            }),
            ..ProcessResponse::default()
        }
    }

    #[test]
    fn evidence_from_status_process_or_title() {
        let by_status = Evidence {
            records: Records::from_reply(&json!({"records":[{"state":"working","app":"claude"}]})),
            ..Evidence::default()
        };
        assert_eq!(by_status.claude_reason(), Some("program status"));
        assert_eq!(by_status.program().as_deref(), Some("claude"));

        let by_script = Evidence {
            process: Some(process(
                "node",
                Some("/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"),
            )),
            ..Evidence::default()
        };
        assert_eq!(by_script.claude_reason(), Some("foreground process"));
        assert_eq!(by_script.program().as_deref(), Some("cli.js"));

        let by_title = Evidence {
            title: Some("✳ Claude Code".into()),
            process: Some(process("bash", None)),
            ..Evidence::default()
        };
        assert_eq!(by_title.claude_reason(), Some("terminal title"));
        assert_eq!(by_title.program().as_deref(), Some("bash"));

        let shell = Evidence {
            process: Some(process("bash", None)),
            ..Evidence::default()
        };
        assert_eq!(shell.claude_reason(), None);
    }

    #[test]
    fn chooses_the_only_or_focused_claude_pane() {
        let panes = panes(&view());
        let claude = || Evidence {
            process: Some(process("claude", None)),
            ..Evidence::default()
        };
        let shell = || Evidence {
            process: Some(process("bash", None)),
            ..Evidence::default()
        };

        let one = vec![(panes[0].clone(), claude()), (panes[1].clone(), shell())];
        assert_eq!(
            choose_claude(&one).unwrap().0.block_id.as_str(),
            "block:aaa"
        );

        let both = vec![(panes[0].clone(), claude()), (panes[1].clone(), claude())];
        assert_eq!(
            choose_claude(&both).unwrap().0.block_id.as_str(),
            "block:abb"
        );

        let mut unfocused = both.clone();
        unfocused[1].0.focused = false;
        assert!(choose_claude(&unfocused).is_err());

        let none = vec![(panes[0].clone(), shell())];
        let error = choose_claude(&none).unwrap_err().to_string();
        assert!(error.contains("block:aaa"), "{error}");
    }
}
