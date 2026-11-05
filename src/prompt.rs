//! Whether a pane's text shows Claude Code's input prompt, the only place `send` types.
//!
//! Current Claude Code draws its prompt as a `❯` line directly under a `────` rule; older
//! versions draw `│ > │` directly under `╭───╮`. Dialogs that take the prompt's place, such as
//! "Do you trust the files in this folder?" or a permission request, show a numbered menu
//! whose cursor line also starts with `❯`. Enter in such a menu picks the highlighted option,
//! so a pasted message followed by Enter could answer "No, exit" or "Yes, allow". Anything
//! that isn't recognizably the prompt is treated as not the prompt.
//!
//! The input box can also hold text nobody sent. Rex's `vt` format keeps text styles, which
//! tell typed text from the placeholder suggestion (`Try "fix lint errors"`) that Claude Code
//! draws faint (SGR 2) in an empty box. Plain text has no styles, so read the `vt` format when
//! the input matters.

use crate::status::Status;

/// How many lines from the bottom of the pane's text are searched.
const BOTTOM: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Claude Code's input prompt, with no menu below it.
    Prompt,
    /// A numbered menu, such as the folder-trust dialog or a permission request.
    Menu,
    /// Neither: a shell, a startup screen, or a layout this check doesn't know.
    Unknown,
}

/// What the bottom of a pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub screen: Screen,
    /// The text in the input box that nobody sent, if the screen is the prompt and the box
    /// isn't empty. Faint text, such as the placeholder, doesn't count.
    pub input: Option<String>,
    /// The menu's dialog, if the screen is a menu.
    pub dialog: Option<Dialog>,
    /// Background shells the footer under the prompt says are still running, such as
    /// `1 shell still running`. Claude Code ends its turn without waiting for them.
    pub shells: usize,
}

impl Reading {
    /// A dialog the program doesn't report in its status, such as the folder-trust dialog
    /// Claude Code shows before its prompt. A permission request reports a blocked status.
    pub fn startup_dialog(&self, status: Status) -> Option<&Dialog> {
        self.dialog.as_ref().filter(|_| status == Status::Idle)
    }
}

/// A dialog with a numbered menu, as the pane shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    /// The dialog's lines without box borders, from its top edge to the bottom of the pane.
    pub text: String,
    /// The folder a trust dialog asks about.
    pub folder: Option<String>,
}

impl Dialog {
    fn parse(lines: &[Line]) -> Self {
        let first_option = lines
            .iter()
            .position(|l| is_menu_line(&l.plain))
            .unwrap_or_default();
        let top = lines[..first_option]
            .iter()
            .rposition(|l| is_rule(&l.plain))
            .map_or(0, |i| i + 1);
        let body: Vec<&str> = lines[top..]
            .iter()
            .filter(|l| !is_edge(&l.plain))
            .map(|l| unboxed(&l.plain))
            .collect();
        let start = body
            .iter()
            .position(|l| !l.is_empty())
            .unwrap_or(body.len());
        let end = body
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(start, |i| i + 1);
        let text = body[start..end].join("\n");
        let folder = text
            .contains("trust")
            .then(|| body.iter().find(|l| l.starts_with(['/', '~'])))
            .flatten()
            .map(|l| l.to_string());
        Dialog { text, folder }
    }

    /// One line about the dialog: the folder a trust dialog names, or its first line.
    pub fn summary(&self) -> String {
        match &self.folder {
            Some(folder) => format!("asks whether to trust {folder}"),
            None => self.text.lines().next().unwrap_or_default().to_owned(),
        }
    }
}

/// Reads the bottom of a pane's text: Rex's `vt` format, or plain text.
pub fn read(screen: &str) -> Reading {
    let mut lines = styled_lines(screen);
    while lines.last().is_some_and(|l| l.plain.is_empty()) {
        lines.pop();
    }
    let bottom = &lines[lines.len().saturating_sub(BOTTOM)..];
    let prompt = (1..bottom.len())
        .rev()
        .find(|&i| is_rule(&bottom[i - 1].plain) && is_prompt_line(&bottom[i].plain));
    if let Some(at) = prompt
        && !bottom[at + 1..].iter().any(|l| is_menu_line(&l.plain))
    {
        let footer = bottom[at + 1..]
            .iter()
            .skip_while(|l| !is_rule(&l.plain) && !is_edge(&l.plain));
        return Reading {
            screen: Screen::Prompt,
            input: input(&bottom[at..]),
            dialog: None,
            shells: footer.filter_map(|l| shell_count(&l.plain)).sum(),
        };
    }
    if bottom.iter().any(|l| is_menu_line(&l.plain)) {
        return Reading {
            screen: Screen::Menu,
            input: None,
            dialog: Some(Dialog::parse(bottom)),
            shells: 0,
        };
    }
    Reading {
        screen: Screen::Unknown,
        input: None,
        dialog: None,
        shells: 0,
    }
}

/// The `N` in a footer line's `N shell(s)`.
fn shell_count(line: &str) -> Option<usize> {
    let words: Vec<&str> = line.split_whitespace().collect();
    words.windows(2).find_map(|w| {
        let noun = w[1].trim_end_matches(|c: char| !c.is_alphanumeric());
        matches!(noun, "shell" | "shells")
            .then(|| w[0].parse().ok())
            .flatten()
    })
}

/// What the bottom of a pane's text shows.
pub fn classify(text: &str) -> Screen {
    read(text).screen
}

/// The typed text from the prompt line down to the box's bottom edge.
fn input(lines: &[Line]) -> Option<String> {
    let first = after_marker(&lines[0].typed).unwrap_or_default();
    let rest = lines[1..]
        .iter()
        .take_while(|l| !is_rule(&l.plain) && !is_edge(&l.plain))
        .map(|l| unboxed(&l.typed));
    let text: Vec<&str> = std::iter::once(first.trim())
        .chain(rest)
        .filter(|l| !l.is_empty())
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

/// One line of the screen, as shown and with faint text blanked out.
#[derive(Debug, Default)]
struct Line {
    plain: String,
    typed: String,
}

/// Splits a screen into lines, following the SGR state for faint text. Other escape sequences
/// are dropped, except cursor-forward, which stands for spaces.
fn styled_lines(screen: &str) -> Vec<Line> {
    let mut lines = vec![Line::default()];
    let mut faint = false;
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        let line = lines.last_mut().expect("starts with a line");
        match c {
            '\n' => {
                line.plain.truncate(line.plain.trim_end().len());
                lines.push(Line::default());
            }
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut last = None;
                    for p in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&p) {
                            last = Some(p);
                            break;
                        }
                        params.push(p);
                    }
                    match last {
                        Some('m') => faint = sgr_faint(&params, faint),
                        Some('C') => {
                            let n = params.parse().unwrap_or(1).max(1);
                            line.plain.extend(std::iter::repeat_n(' ', n));
                            line.typed.extend(std::iter::repeat_n(' ', n));
                        }
                        _ => {}
                    }
                }
                Some(']') => {
                    // An OSC string, such as a hyperlink, ends with BEL or ESC \.
                    while let Some(p) = chars.next() {
                        if p == '\x07' || (p == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                Some('(' | ')') => {
                    chars.next();
                }
                _ => {}
            },
            c if c.is_control() => {}
            c => {
                line.plain.push(c);
                line.typed.push(if faint { ' ' } else { c });
            }
        }
    }
    let line = lines.last_mut().expect("starts with a line");
    line.plain.truncate(line.plain.trim_end().len());
    lines
}

/// The faint attribute after an SGR sequence's parameters.
fn sgr_faint(params: &str, mut faint: bool) -> bool {
    let params: Vec<&str> = params.split(';').collect();
    let mut i = 0;
    while i < params.len() {
        match params[i] {
            "" | "0" | "22" => faint = false,
            "2" => faint = true,
            // Extended colors carry their own numbers, which aren't attributes.
            "38" | "48" | "58" => {
                i += match params.get(i + 1) {
                    Some(&"5") => 2,
                    Some(&"2") => 4,
                    _ => 0,
                };
            }
            _ => {}
        }
        i += 1;
    }
    faint
}

/// The text inside a box border, if the line has one.
fn unboxed(line: &str) -> &str {
    let line = line.trim();
    let line = line.strip_prefix('│').unwrap_or(line);
    line.strip_suffix('│').unwrap_or(line).trim()
}

/// `╰────╯`: the bottom edge of the older boxed prompt.
fn is_edge(line: &str) -> bool {
    let inner = line
        .trim()
        .trim_start_matches(['╰', '└'])
        .trim_end_matches(['╯', '┘']);
    inner.chars().count() >= 3 && inner.chars().all(|c| c == '─')
}

/// `────` or `╭────╮`.
fn is_rule(line: &str) -> bool {
    let inner = line
        .trim()
        .trim_start_matches(['╭', '┌'])
        .trim_end_matches(['╮', '┐']);
    inner.chars().count() >= 3 && inner.chars().all(|c| c == '─')
}

/// The marker, and what follows it, if `line` starts with `❯` or `>`.
fn after_marker(line: &str) -> Option<&str> {
    let text = unboxed(line);
    text.strip_prefix('❯').or_else(|| text.strip_prefix('>'))
}

/// `❯ 1. Yes, proceed`: a menu's highlighted option.
fn is_menu_line(line: &str) -> bool {
    after_marker(line).is_some_and(starts_numbered)
}

/// `❯ ` or `❯ some typed text`, but not a menu option.
fn is_prompt_line(line: &str) -> bool {
    after_marker(line).is_some_and(|rest| {
        (rest.is_empty() || rest.starts_with(char::is_whitespace)) && !starts_numbered(rest)
    })
}

fn starts_numbered(text: &str) -> bool {
    let text = text.trim_start();
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && text[digits..].starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "────────────────────────────────────────";

    fn current(input: &[&str]) -> String {
        let mut lines = vec!["● Fixed it.", "", "✳ Thinking… (3s)", "", RULE];
        lines.extend(input);
        lines.extend([
            RULE,
            "  Opus ╱ 12%",
            "  ⏵⏵ auto mode on (shift+tab to cycle)",
        ]);
        lines.join("\n")
    }

    #[test]
    fn recognizes_the_current_prompt() {
        assert_eq!(classify(&current(&["❯ "])), Screen::Prompt);
        assert_eq!(classify(&current(&["❯ fix the bug"])), Screen::Prompt);
        assert_eq!(
            classify(&current(&["❯ first line", "  second line"])),
            Screen::Prompt
        );
    }

    #[test]
    fn recognizes_the_boxed_prompt() {
        let screen = "Welcome\n\n╭────────╮\n│ >      │\n╰────────╯\n  ? for shortcuts\n\n";
        assert_eq!(classify(screen), Screen::Prompt);
    }

    #[test]
    fn the_trust_dialog_is_a_menu() {
        let boxed = [
            "╭──────────────────────────────────────────╮",
            "│                                          │",
            "│ Do you trust the files in this folder?   │",
            "│                                          │",
            "│ /srv/app                                 │",
            "│                                          │",
            "│ ❯ 1. Yes, proceed                        │",
            "│   2. No, exit                            │",
            "│                                          │",
            "╰──────────────────────────────────────────╯",
            "   Enter to confirm · Esc to exit",
        ];
        assert_eq!(classify(&boxed.join("\n")), Screen::Menu);

        let plain = [
            RULE,
            " Do you trust the files in this folder?",
            "",
            " ❯ 1. Yes, I trust this folder",
            "   2. No, exit",
            "",
            " Enter to confirm · Esc to cancel",
        ];
        assert_eq!(classify(&plain.join("\n")), Screen::Menu);
    }

    #[test]
    fn reads_the_trust_dialog_and_its_folder() {
        let boxed = [
            "● earlier output",
            "╭──────────────────────────────────────────╮",
            "│ Do you trust the files in this folder?   │",
            "│                                          │",
            "│ /srv/app                                 │",
            "│                                          │",
            "│ ❯ 1. Yes, proceed                        │",
            "│   2. No, exit                            │",
            "╰──────────────────────────────────────────╯",
            "   Enter to confirm · Esc to exit",
            "",
        ];
        let dialog = read(&boxed.join("\n")).dialog.unwrap();
        assert_eq!(
            dialog.text,
            "Do you trust the files in this folder?\n\n/srv/app\n\n❯ 1. Yes, proceed\n2. No, exit\nEnter to confirm · Esc to exit"
        );
        assert_eq!(dialog.folder.as_deref(), Some("/srv/app"));
        assert_eq!(dialog.summary(), "asks whether to trust /srv/app");

        let current = [
            "Welcome to Claude Code",
            RULE,
            " Accessing workspace:",
            "",
            " ~/src/app",
            "",
            " Quick safety check: Is this a project you created or one you trust?",
            "",
            " ❯ 1. Yes, I trust this folder",
            "   2. No, exit",
        ];
        let dialog = read(&current.join("\n")).dialog.unwrap();
        assert!(
            dialog
                .text
                .starts_with("Accessing workspace:\n\n~/src/app\n")
        );
        assert_eq!(dialog.folder.as_deref(), Some("~/src/app"));
    }

    #[test]
    fn only_unreported_menus_are_startup_dialogs() {
        let menu = read(" Pick a theme\n ❯ 1. Dark\n   2. Light");
        let dialog = menu.startup_dialog(Status::Idle).unwrap();
        assert_eq!(dialog.folder, None);
        assert_eq!(dialog.summary(), "Pick a theme");
        assert_eq!(menu.startup_dialog(Status::NeedsApproval), None);
        assert_eq!(read(&current(&["❯ "])).startup_dialog(Status::Idle), None);
    }

    #[test]
    fn a_menu_below_the_prompt_wins() {
        let screen = current(&[
            "❯ ",
            "",
            " Allow Bash(cargo test)?",
            " ❯ 1. Yes",
            "   2. No",
        ]);
        assert_eq!(classify(&screen), Screen::Menu);
    }

    #[test]
    fn needs_the_rule_above_the_prompt() {
        assert_eq!(classify("$ ls\nsrc\n$ "), Screen::Unknown);
        assert_eq!(classify("> quoted text in output"), Screen::Unknown);
        assert_eq!(classify(""), Screen::Unknown);
        // A typed message that begins like a menu option isn't trusted either.
        assert_eq!(classify(&current(&["❯ 1. do this"])), Screen::Menu);
    }

    /// The bottom of a real Claude Code screen in Rex's vt format, with `input` on the prompt
    /// line.
    fn vt(input: &str) -> String {
        let rule = format!("\x1b[0m\x1b[38;2;136;136;136m{}\x1b[0m", "─".repeat(40));
        [
            "\x1b[0m\x1b[38;2;153;153;153m  ⎿ \u{a0}Tip: Use /btw to ask a quick side question\x1b[0m",
            "\x1b[2C                              ",
            &rule,
            &format!("\x1b[0m❯\u{a0}{input}"),
            &rule,
            "  \x1b[0m\x1b[38;2;208;135;112mOpus 5.5\x1b[0m\x1b[38;2;153;153;153m \x1b[0m\x1b[38;2;129;161;193m0%\x1b[0m",
            "  \x1b[0m\x1b[38;2;255;193;7m⏵⏵ auto mode on\x1b[0m\x1b[40;3H",
        ]
        .join("\r\n")
    }

    #[test]
    fn reads_typed_input_but_not_the_placeholder() {
        assert_eq!(
            read(&vt("")),
            Reading {
                screen: Screen::Prompt,
                input: None,
                dialog: None,
                shells: 0,
            }
        );
        let placeholder = vt("\x1b[0m\x1b[2mTry \"fix lint errors\"\x1b[0m");
        assert_eq!(read(&placeholder).input, None);
        assert_eq!(read(&placeholder).screen, Screen::Prompt);
        let typed = vt("leftover\x1b[1Cwords");
        assert_eq!(read(&typed).input.as_deref(), Some("leftover words"));
    }

    #[test]
    fn reads_multi_line_and_boxed_input() {
        let screen = current(&["❯ first line", "  second line"]);
        assert_eq!(
            read(&screen).input.as_deref(),
            Some("first line\nsecond line")
        );
        let boxed = "╭────────╮\n│ > half │\n│   more │\n╰────────╯\n  ? for shortcuts";
        assert_eq!(read(boxed).input.as_deref(), Some("half\nmore"));
        let empty = "╭────────╮\n│ >      │\n╰────────╯\n  ? for shortcuts";
        assert_eq!(read(empty).input, None);
    }

    #[test]
    fn faint_survives_extended_colors() {
        assert!(sgr_faint("2", false));
        assert!(!sgr_faint("0", true));
        assert!(!sgr_faint("22", true));
        // The 2 in 38;2;r;g;b is a color mode, not faint.
        assert!(!sgr_faint("38;2;153;153;153", false));
        assert!(sgr_faint("38;2;1;2;3;2", false));
        assert!(!sgr_faint("38;5;2", false));
    }

    #[test]
    fn hyperlinks_and_charsets_are_skipped() {
        let lines = styled_lines("a\x1b]8;;https://x\x1b\\link\x1b]8;;\x07b\x1b(Bc");
        assert_eq!(lines[0].plain, "alinkbc");
    }

    #[test]
    fn only_the_bottom_counts() {
        let mut screen = current(&["❯ "]);
        screen.push_str(&"\noutput".repeat(BOTTOM));
        assert_eq!(classify(&screen), Screen::Unknown);
    }

    #[test]
    fn counts_background_shells_in_the_footer_only() {
        let with_footer = |input: &str, footer: &str| {
            let mut screen = current(&["● Started 3 shells in the background.", RULE, input]);
            screen.push_str(&format!("\n  {footer}"));
            read(&screen).shells
        };
        assert_eq!(with_footer("❯ ", "? for shortcuts"), 0);
        assert_eq!(with_footer("❯ ", "1 shell still running"), 1);
        assert_eq!(
            with_footer("❯ ", "⏵⏵ auto mode on · 3 shells still running"),
            3
        );
        assert_eq!(with_footer("❯ kill 2 shells", "? for shortcuts"), 0);
        assert_eq!(
            read("$ echo 1 shell still running\n1 shell still running").shells,
            0
        );
    }
}
