//! Whether a pane's text shows Claude Code's input prompt, the only place `send` types.
//!
//! Current Claude Code draws its prompt as a `❯` line directly under a `────` rule; older
//! versions draw `│ > │` directly under `╭───╮`. Dialogs that take the prompt's place, such as
//! "Do you trust the files in this folder?" or a permission request, show a numbered menu
//! whose cursor line also starts with `❯`. Enter in such a menu picks the highlighted option,
//! so a pasted message followed by Enter could answer "No, exit" or "Yes, allow". Anything
//! that isn't recognizably the prompt is treated as not the prompt.

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

/// Reads the bottom of a pane's text.
pub fn classify(text: &str) -> Screen {
    let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let bottom = &lines[lines.len().saturating_sub(BOTTOM)..];
    let prompt = (1..bottom.len())
        .rev()
        .find(|&i| is_rule(bottom[i - 1]) && is_prompt_line(bottom[i]));
    if let Some(at) = prompt
        && !bottom[at + 1..].iter().any(|l| is_menu_line(l))
    {
        return Screen::Prompt;
    }
    if bottom.iter().any(|l| is_menu_line(l)) {
        Screen::Menu
    } else {
        Screen::Unknown
    }
}

/// The text inside a box border, if the line has one.
fn unboxed(line: &str) -> &str {
    let line = line.trim();
    let line = line.strip_prefix('│').unwrap_or(line);
    line.strip_suffix('│').unwrap_or(line).trim()
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

    #[test]
    fn only_the_bottom_counts() {
        let mut screen = current(&["❯ "]);
        screen.push_str(&"\noutput".repeat(BOTTOM));
        assert_eq!(classify(&screen), Screen::Unknown);
    }
}
