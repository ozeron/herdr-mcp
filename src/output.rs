/// Removes terminal escape sequences and control characters other than newline and tab.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Keeps the last `max_chars` characters, since the end of a terminal is its most recent output.
pub fn truncate_tail(text: String, max_chars: usize) -> (String, bool) {
    let total = text.chars().count();
    if total <= max_chars {
        return (text, false);
    }
    let start = text
        .char_indices()
        .nth(total - max_chars)
        .map_or(text.len(), |(index, _)| index);
    (text[start..].to_string(), true)
}

pub fn truncate_head(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}
