//! Bounded formatting for system and diagnostic log messages.

/// Maximum number of characters in one system or diagnostic log event.
pub const DIAGNOSTIC_EVENT_LIMIT: usize = 500;

/// Which part of an oversized diagnostic to retain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepPart {
    Prefix,
    Suffix,
}

/// Make a diagnostic one line and truncate it to `limit` characters.
///
/// The truncation marker reports how many normalized characters were omitted.
pub fn format_bounded_diagnostic(value: &str, limit: usize, keep: KeepPart) -> String {
    let normalized = normalize_diagnostic(value);
    let chars = normalized.chars().collect::<Vec<_>>();
    if chars.len() <= limit {
        return normalized;
    }

    if limit == 0 {
        return String::new();
    }
    if limit < "… [truncated 0 chars]".chars().count() {
        return "…".chars().take(limit).collect();
    }

    let mut kept = limit.saturating_sub("… [truncated 0 chars]".chars().count());
    let marker = loop {
        let omitted = chars.len().saturating_sub(kept);
        let marker = format!("… [truncated {omitted} chars]");
        let next_kept = limit.saturating_sub(marker.chars().count());
        if next_kept == kept {
            break marker;
        }
        kept = next_kept;
    };

    let content = match keep {
        KeepPart::Prefix => chars.iter().take(kept).collect::<String>(),
        KeepPart::Suffix => chars[chars.len().saturating_sub(kept)..]
            .iter()
            .collect::<String>(),
    };
    match keep {
        KeepPart::Prefix => format!("{content}{marker}"),
        KeepPart::Suffix => format!("{marker}{content}"),
    }
}

fn normalize_diagnostic(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut previous_was_separator = false;
    for character in value.chars() {
        let is_separator = matches!(character, '\r' | '\n');
        if is_separator {
            if !previous_was_separator {
                normalized.push_str(" | ");
            }
        } else if character.is_control() {
            normalized.push(' ');
        } else {
            normalized.push(character);
        }
        previous_was_separator = is_separator;
    }
    normalized.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::{DIAGNOSTIC_EVENT_LIMIT, KeepPart, format_bounded_diagnostic};

    #[test]
    fn truncates_system_diagnostics_with_an_explicit_omission_count() {
        let message = "diagnostic".repeat(100);
        let bounded = format_bounded_diagnostic(&message, DIAGNOSTIC_EVENT_LIMIT, KeepPart::Prefix);

        assert_eq!(bounded.chars().count(), DIAGNOSTIC_EVENT_LIMIT);
        assert!(bounded.starts_with("diagnostic"));
        assert!(bounded.contains("[truncated "));
        assert!(bounded.ends_with(" chars]"));
    }

    #[test]
    fn suffix_truncation_retains_the_end_of_a_diagnostic() {
        let bounded = format_bounded_diagnostic(
            &format!("{}fatal: remote rejected the request", "x".repeat(900)),
            80,
            KeepPart::Suffix,
        );

        assert_eq!(bounded.chars().count(), 80);
        assert!(bounded.starts_with("… [truncated "));
        assert!(bounded.ends_with("fatal: remote rejected the request"));
    }

    #[test]
    fn normalizes_newlines_and_carriage_returns_to_one_log_line() {
        assert_eq!(
            format_bounded_diagnostic("first\rprogress\nsecond", 100, KeepPart::Prefix),
            "first | progress | second"
        );
    }
}
