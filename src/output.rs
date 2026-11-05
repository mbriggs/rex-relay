//! Turns repeated snapshots of a pane's text into a stream of new lines.
//!
//! Rex gives a terminal's screen and scrollback as text. A full-screen program such as Claude
//! Code keeps redrawing the bottom of the screen (its prompt box, spinner and footer), so the
//! last few lines are held back until they stop being the bottom, or until the stream ends.

/// Lines at the bottom of the screen that are still likely to be redrawn.
pub const DEFAULT_HOLDBACK: usize = 8;

#[derive(Debug, Clone)]
pub struct Pump {
    /// The text when the stream started.
    base: Vec<String>,
    last: Vec<String>,
    /// How many lines of `last` count as already printed (or as there before we started).
    emitted: usize,
    holdback: usize,
}

fn lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(|l| l.trim_end().to_owned()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

impl Pump {
    /// Starts from `baseline`, which counts as already seen.
    pub fn new(baseline: &str, holdback: usize) -> Self {
        let last = lines(baseline);
        // The baseline's own bottom lines are the prompt box and footer; anything that later
        // replaces them is new.
        let emitted = last.len().saturating_sub(holdback);
        Pump {
            base: last.clone(),
            last,
            emitted,
            holdback,
        }
    }

    /// The new stable lines in `text`.
    pub fn update(&mut self, text: &str) -> Vec<String> {
        let next = lines(text);
        self.resync(&next);
        let stable = next.len().saturating_sub(self.holdback);
        self.skip_unchanged(&next, stable);
        let out = if stable > self.emitted {
            next[self.emitted..stable].to_vec()
        } else {
            Vec::new()
        };
        self.emitted = self.emitted.max(stable);
        self.last = next;
        out
    }

    /// Every line not yet printed, the held-back ones included.
    pub fn finish(&mut self, text: &str) -> Vec<String> {
        let next = lines(text);
        self.resync(&next);
        self.skip_unchanged(&next, next.len());
        let out = next.get(self.emitted..).unwrap_or_default().to_vec();
        self.emitted = next.len();
        self.last = next;
        out
    }

    /// Lines that were held back in the baseline and still read the same weren't produced by
    /// this run, so they are never printed.
    fn skip_unchanged(&mut self, next: &[String], limit: usize) {
        let limit = limit.min(self.base.len()).min(next.len());
        while self.emitted < limit && next[self.emitted] == self.base[self.emitted] {
            self.emitted += 1;
        }
    }

    /// When text above the printed point changed (a clear, a redraw, scrollback trimmed),
    /// finds where the printed part now ends, so that nothing printed is printed again.
    fn resync(&mut self, next: &[String]) {
        let common = self
            .last
            .iter()
            .zip(next)
            .take_while(|(a, b)| a == b)
            .count();
        if common >= self.emitted {
            return;
        }
        // Find the last printed lines again, searching from the bottom: trimmed scrollback
        // moves them up. Longer runs are tried first, as they are less likely to match by
        // chance.
        let end = self.emitted;
        for run in (1..=end.min(3)).rev() {
            let tail = &self.last[end - run..end];
            if let Some(at) = next
                .windows(run)
                .rposition(|w| w == tail && w.iter().any(|l| !l.is_empty()))
            {
                self.emitted = at + run;
                return;
            }
        }
        self.emitted = common;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(body: &[&str]) -> String {
        let mut lines: Vec<&str> = body.to_vec();
        lines.extend(["", "╭─────╮", "│ >   │", "╰─────╯", "  ? for shortcuts"]);
        lines.join("\n")
    }

    #[test]
    fn streams_lines_as_they_scroll_out_of_the_holdback() {
        let mut pump = Pump::new(&screen(&["old output"]), 5);
        assert!(pump.update(&screen(&["old output"])).is_empty());
        let out = pump.update(&screen(&["old output", "> fix it", "● Reading files"]));
        assert_eq!(out, ["> fix it", "● Reading files"]);
        let out = pump.update(&screen(&[
            "old output",
            "> fix it",
            "● Reading files",
            "● Done.",
        ]));
        assert_eq!(out, ["● Done."]);
        let rest = pump.finish(&screen(&[
            "old output",
            "> fix it",
            "● Reading files",
            "● Done.",
        ]));
        assert_eq!(
            rest,
            ["", "╭─────╮", "│ >   │", "╰─────╯", "  ? for shortcuts"]
        );
    }

    #[test]
    fn redraws_inside_the_holdback_are_not_printed() {
        let mut pump = Pump::new("a\nb", 2);
        assert!(pump.update("a\nb\n✻ Thinking").is_empty());
        let out = pump.update("a\nb\nc\n✻ Thinking.\nfooter");
        assert_eq!(out, ["c"], "b was already on screen");
        let out = pump.update("a\nb\nc\n✻ Thinking..\nfooter");
        assert!(out.is_empty());
    }

    #[test]
    fn trimmed_scrollback_does_not_repeat_lines() {
        let mut pump = Pump::new("", 0);
        assert_eq!(pump.update("1\n2\n3\n4"), ["1", "2", "3", "4"]);
        // The oldest lines fell off the top of the scrollback.
        assert_eq!(pump.update("3\n4\n5"), ["5"]);
    }

    #[test]
    fn a_cleared_screen_starts_over() {
        let mut pump = Pump::new("", 0);
        assert_eq!(pump.update("x\ny"), ["x", "y"]);
        assert_eq!(pump.update("fresh"), ["fresh"]);
    }

    #[test]
    fn trailing_spaces_and_blank_lines_are_ignored() {
        let mut pump = Pump::new("a   \n\n\n", 0);
        assert!(pump.update("a\n").is_empty());
        assert_eq!(pump.update("a\nb  \n\n"), ["b"]);
    }
}
