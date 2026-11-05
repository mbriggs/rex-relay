//! Command-line arguments.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "rexctl",
    version,
    about = "Drive Claude Code panes in Rex sessions, with guard rails",
    after_help = "Exit codes: 0 done/ok, 1 error, 2 usage, 3 refused by a guard rail, \
                  10 needs approval, 11 question, 12 needs input, 13 needs sign-in, \
                  14 program error, 15 timeout, 16 lost connection."
)]
pub struct Cli {
    /// The Rex server, such as https://rex-host.example.ts.net. Overrides REX_SERVER and the
    /// config's `endpoint`. Must be in the config's `allowed_endpoints`.
    #[arg(long, global = true, value_name = "URL")]
    pub endpoint: Option<String>,

    /// The config file. Defaults to $REX_RELAY_CONFIG, then ~/.config/rex-relay/config.toml.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Print line-delimited JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List the server's sessions, marking the ones rexctl created.
    List,
    /// Show sessions, their tabs, and their panes, with what runs in each pane and its status.
    Tree {
        /// Only this session: its id or current label.
        session: Option<String>,
    },
    /// Show the status of a session's Claude Code pane.
    Status(Target),
    /// Print the last lines of a session's Claude Code pane.
    Read {
        #[command(flatten)]
        target: Target,
        /// How many lines to print from the end. 0 prints everything.
        #[arg(long, default_value_t = 100, value_name = "N")]
        lines: usize,
    },
    /// Create a session that runs Claude Code, and record its id as owned by rexctl.
    New {
        /// The session's initial label. Claude Code may rename the session later; rexctl tracks
        /// it by id. Without a label, Rex picks one.
        label: Option<String>,
        /// The directory the pane starts in, on the Rex host.
        #[arg(long, value_name = "DIR")]
        cwd: Option<String>,
        /// The program to run and its arguments. Quote arguments as a shell would, such as
        /// --cmd "claude --append-system-prompt 'Be terse.'"; no shell runs it.
        #[arg(long, default_value = "claude", value_parser = parse_program, value_name = "COMMAND")]
        cmd: Program,
        /// What the session is for, kept in the ownership registry and shown by `list`.
        #[arg(long, value_name = "TEXT")]
        purpose: Option<String>,
    },
    /// Send a message to Claude Code, then stream its output until it stops.
    Send(SendArgs),
    /// Stream a session's Claude Code output until it stops working.
    Wait(WaitArgs),
    /// Type a command into a shell pane idle at its prompt, wait for the prompt to return, and
    /// print the new output.
    Run(RunArgs),
    /// Close a session that rexctl created. Sessions rexctl didn't create are never closed.
    Close {
        /// The session's id (session:…) or current label.
        session: String,
    },
}

#[derive(Debug, Clone, Args)]
pub struct Target {
    /// The session, by id (session:…) or current label, optionally followed by a tab and a
    /// pane: session:tab or session:tab:pane, such as solar-creek:shell. Tabs and panes match
    /// by label, id, or number; see `rexctl tree`.
    #[arg(value_name = "TARGET")]
    pub name: String,
    /// The block to use instead of the one that runs Claude Code: its id, or a unique prefix.
    #[arg(long, value_name = "ID")]
    pub block: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct SendArgs {
    #[command(flatten)]
    pub target: Target,
    /// The message. Newlines and tabs are allowed; other control characters are not.
    pub message: String,
    /// Send to a session rexctl didn't create (one not in the ownership registry).
    #[arg(long)]
    pub allow_foreign: bool,
    /// The message answers a pending approval or question. Without it, send refuses while
    /// the program is in Needs approval or Question. A question shown as a numbered menu is
    /// still refused: send only types at Claude Code's input prompt.
    #[arg(long)]
    pub answer: bool,
    /// If Claude Code's input box already holds unsent text, clear it before typing. Without
    /// this, send refuses. Clearing uses Ctrl+E and Ctrl+U, which edit the input and never
    /// interrupt Claude Code.
    #[arg(long)]
    pub clear_input: bool,
    /// Lowers the largest message allowed, in bytes, below the config's `max_message_bytes`
    /// (default 32768). It can't raise it.
    #[arg(long, value_name = "BYTES")]
    pub max_bytes: Option<usize>,
    #[command(flatten)]
    pub wait: WaitOptions,
}

#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub target: Target,
    /// Use a session rexctl didn't create (one not in the ownership registry).
    #[arg(long)]
    pub allow_foreign: bool,
    /// Lowers the largest command allowed, in bytes, below the config's `max_message_bytes`.
    #[arg(long, value_name = "BYTES")]
    pub max_bytes: Option<usize>,
    #[command(flatten)]
    pub wait: WaitOptions,
    /// The command line, after `--`. Its words are joined with spaces and typed as one line,
    /// as ssh does, so quote anything the shell should see quoted: -- 'ls | wc -l'.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct WaitArgs {
    #[command(flatten)]
    pub target: Target,
    #[command(flatten)]
    pub wait: WaitOptions,
}

#[derive(Debug, Clone, Args)]
pub struct WaitOptions {
    /// How long to wait for the program to stop, such as 90s or 30m. Defaults to the
    /// config's, or 30m.
    #[arg(long, value_parser = humantime::parse_duration, value_name = "DURATION")]
    pub timeout: Option<Duration>,
    /// Don't stream the pane's output or status changes. At the end, print only the last lines
    /// of the pane (see --lines) and the final status.
    #[arg(long)]
    pub quiet: bool,
    /// With --quiet, how many lines from the end of the pane to print at the end. 0 prints
    /// everything.
    #[arg(long, default_value_t = 20, value_name = "N")]
    pub lines: usize,
    /// How often to read the pane's text, such as 500ms.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "1s", value_name = "DURATION")]
    pub poll: Duration,
}

/// A program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program(pub Vec<String>);

/// Splits `--cmd` into words with POSIX shell quoting rules.
fn parse_program(text: &str) -> Result<Program, String> {
    match shlex::split(text) {
        Some(words) if !words.is_empty() => Ok(Program(words)),
        Some(_) => Err("the command is empty".into()),
        None => Err("the command has an unclosed quote or a trailing backslash".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_split_like_a_shell() {
        assert_eq!(
            parse_program("claude --append-system-prompt 'Be terse.' --model \"opus 5\"").unwrap(),
            Program(
                [
                    "claude",
                    "--append-system-prompt",
                    "Be terse.",
                    "--model",
                    "opus 5"
                ]
                .map(String::from)
                .to_vec()
            )
        );
        for bad in ["", "   ", "claude 'unclosed", "claude \\"] {
            assert!(parse_program(bad).is_err(), "{bad:?}");
        }
    }
}
