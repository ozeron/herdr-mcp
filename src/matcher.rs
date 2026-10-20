use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use crate::output::{strip_ansi, truncate_tail};

pub const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Herdr's cap for `pane.read`; reading all of it keeps a fast scroll from hiding the match.
pub const READ_LINES: u32 = 1000;
const EXCERPT_CONTEXT: usize = 4;
const TAIL_LINES: usize = 20;
const TAIL_MAX_CHARS: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOutcome {
    Matched { excerpt: String },
    TimedOut { tail: String },
}

/// Matches `needle` only in lines added after the baseline, skipping the echoed command.
#[derive(Debug, Clone)]
pub struct Matcher {
    baseline: Vec<String>,
    needle: String,
    echo: Vec<String>,
    /// Last line of a command; only lines after its last echo count.
    anchor: Option<String>,
}

impl Matcher {
    pub fn new(baseline: &str, needle: &str, command: &str) -> Self {
        Self {
            baseline: split_lines(baseline),
            needle: needle.to_string(),
            echo: echo_lines(command),
            anchor: None,
        }
    }

    /// Considers only lines after the last echo of `command`, not the screen at call time.
    /// A line already printed after that echo matches at once.
    pub fn after_command(needle: &str, command: &str) -> Self {
        let echo = echo_lines(command);
        Self {
            baseline: Vec::new(),
            needle: needle.to_string(),
            anchor: echo.last().cloned(),
            echo,
        }
    }

    /// Whether this matcher is anchored and the echoed command is absent from `current`.
    pub fn anchor_missing(&self, current: &str) -> bool {
        self.anchor.is_some() && self.anchored(&split_lines(current)).is_none()
    }

    fn anchored(&self, current: &[String]) -> Option<Vec<String>> {
        let anchor = self.anchor.as_deref()?;
        let at = current.iter().rposition(|line| line.contains(anchor))?;
        Some(current[at + 1..].to_vec())
    }

    /// Lines of `current` that were not in the baseline.
    ///
    /// The baseline is aligned to the start of `current` at the largest overlap, which covers a
    /// buffer that slid past its first lines. The last baseline line is the cursor line and may
    /// have grown (a prompt that now holds the typed command), so it is old only if unchanged.
    pub fn new_lines(&self, current: &str) -> Vec<String> {
        let current = split_lines(current);
        let base = &self.baseline;
        if base.is_empty() {
            return current;
        }
        for skip in 0..base.len() {
            let overlap = base.len() - skip;
            if overlap > current.len() {
                continue;
            }
            let stable = overlap - 1;
            if base[skip..skip + stable] != current[..stable] {
                continue;
            }
            let last_unchanged = current.get(stable) == base.last();
            let start = if last_unchanged { overlap } else { stable };
            return current[start..].to_vec();
        }
        current
    }

    /// The first new line containing the needle, with a few lines of context before it.
    pub fn find(&self, current: &str) -> Option<String> {
        let lines = match self.anchor {
            Some(_) => self.anchored(&split_lines(current)).unwrap_or_default(),
            None => self.new_lines(current),
        };
        let fresh: Vec<String> = lines
            .into_iter()
            .filter(|line| !self.is_echo(line))
            .collect();
        let hit = fresh.iter().position(|line| line.contains(&self.needle))?;
        let from = hit.saturating_sub(EXCERPT_CONTEXT);
        Some(fresh[from..=hit].join("\n"))
    }

    fn is_echo(&self, line: &str) -> bool {
        self.echo
            .iter()
            .any(|command| line.contains(command.as_str()))
    }
}

fn echo_lines(command: &str) -> Vec<String> {
    command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect()
}

fn split_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = strip_ansi(text)
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// Polls `read` until the matcher finds a new line or `timeout` passes. Always reads at least once.
pub async fn wait_for_match<E, F, Fut>(
    matcher: &Matcher,
    timeout: Duration,
    interval: Duration,
    mut read: F,
) -> Result<MatchOutcome, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String, E>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        let text = read().await?;
        if let Some(excerpt) = matcher.find(&text) {
            return Ok(MatchOutcome::Matched { excerpt });
        }
        if Instant::now() >= deadline {
            return Ok(MatchOutcome::TimedOut { tail: tail(&text) });
        }
        tokio::time::sleep(interval.min(deadline - Instant::now())).await;
    }
}

/// The last meaningful lines of `text`. Blank lines and TUI rules (box-drawing only) are skipped.
pub fn tail(text: &str) -> String {
    let lines: Vec<_> = split_lines(text)
        .into_iter()
        .filter(|line| crate::output::is_meaningful(line))
        .collect();
    let from = lines.len().saturating_sub(TAIL_LINES);
    truncate_tail(lines[from..].join("\n"), TAIL_MAX_CHARS).0
}
