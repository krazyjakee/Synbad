//! Stderr-line parser for the Synergy/Deskflow Core.
//!
//! The Core does have a binary IPC channel for structured status, but the
//! protocol is bespoke and version-tied. Since both Synergy and Deskflow emit
//! the same human-readable log format on stderr, we recognize a small set of
//! lines and surface them as structured [`crate::Event`]s alongside the raw
//! log feed.
//!
//! Lines we currently understand (Synergy 1.x and Deskflow inherit this
//! format from the original synergy-core sources):
//!
//! ```text
//! NOTE: client "<name>" has connected
//! NOTE: client "<name>" has disconnected
//! NOTE: switch from "<from>" to "<to>"
//! ```
//!
//! Leading log level + timestamp prefixes (`NOTE: `, `2026-04-01T12:34:56 NOTE:`,
//! a debug build's `DEBUG1: `) are stripped before matching.

use crate::Event;

/// Try to extract a structured event from a single line of Core stderr.
/// Returns `None` for lines that don't carry a recognized signal — those
/// still flow through as raw `Event::Log`.
pub fn parse(line: &str) -> Option<Event> {
    let body = strip_log_prefix(line);

    if let Some(name) = extract_quoted_after(body, "client ", " has connected") {
        return Some(Event::PeerConnected {
            name: name.to_string(),
        });
    }
    if let Some(name) = extract_quoted_after(body, "client ", " has disconnected") {
        return Some(Event::PeerDisconnected {
            name: name.to_string(),
        });
    }
    if let Some((_, to)) = extract_switch(body) {
        return Some(Event::ActiveScreen {
            name: to.to_string(),
        });
    }
    None
}

/// Strip optional ISO-ish timestamp and the `LEVEL: ` log prefix the Core
/// emits, returning the message body. Tolerant — unknown prefixes pass
/// through unchanged.
fn strip_log_prefix(line: &str) -> &str {
    // Drop a leading bracketed timestamp like "[2026-05-19T12:34:56]" or
    // a bare "2026-05-19T12:34:56" prefix.
    let trimmed = line.trim_start();
    let after_ts = if let Some(rest) = trimmed.strip_prefix('[') {
        rest.find(']')
            .map(|i| rest[i + 1..].trim_start())
            .unwrap_or(trimmed)
    } else if trimmed
        .chars()
        .take(10)
        .all(|c| c.is_ascii_digit() || c == '-')
    {
        // Heuristic: a date-shaped prefix followed by a space.
        trimmed
            .find(' ')
            .map(|i| trimmed[i + 1..].trim_start())
            .unwrap_or(trimmed)
    } else {
        trimmed
    };

    // Strip `LEVEL: ` (NOTE, DEBUG, DEBUG1..N, WARNING, ERROR, FATAL, INFO).
    for prefix in [
        "NOTE: ",
        "WARNING: ",
        "ERROR: ",
        "FATAL: ",
        "INFO: ",
        "DEBUG: ",
        "DEBUG1: ",
        "DEBUG2: ",
    ] {
        if let Some(rest) = after_ts.strip_prefix(prefix) {
            return rest;
        }
    }
    after_ts
}

/// Find `prefix "<NAME>" suffix` and return `<NAME>` borrowed from `body`.
fn extract_quoted_after<'a>(body: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    let after_prefix = body.strip_prefix(prefix)?;
    let after_quote = after_prefix.strip_prefix('"')?;
    let close = after_quote.find('"')?;
    let (name, rest) = after_quote.split_at(close);
    rest.strip_prefix('"')?.strip_prefix(suffix)?;
    Some(name)
}

/// Recognize `switch from "<from>" to "<to>"`.
fn extract_switch(body: &str) -> Option<(&str, &str)> {
    let rest = body.strip_prefix("switch from \"")?;
    let from_end = rest.find('"')?;
    let (from, after_from) = rest.split_at(from_end);
    let to_block = after_from.strip_prefix("\" to \"")?;
    let to_end = to_block.find('"')?;
    let (to, _) = to_block.split_at(to_end);
    Some((from, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_connect_and_disconnect() {
        let conn = parse(r#"NOTE: client "laptop" has connected"#).unwrap();
        match conn {
            Event::PeerConnected { name } => assert_eq!(name, "laptop"),
            _ => panic!("wrong event: {:?}", conn),
        }
        let dc = parse(r#"NOTE: client "laptop" has disconnected"#).unwrap();
        match dc {
            Event::PeerDisconnected { name } => assert_eq!(name, "laptop"),
            _ => panic!(),
        }
    }

    #[test]
    fn recognizes_switch() {
        let sw = parse(r#"NOTE: switch from "desktop" to "laptop""#).unwrap();
        match sw {
            Event::ActiveScreen { name } => assert_eq!(name, "laptop"),
            _ => panic!(),
        }
    }

    #[test]
    fn tolerates_iso_timestamp_prefix() {
        let sw = parse(r#"2026-05-19T12:34:56 NOTE: client "phone" has connected"#).unwrap();
        match sw {
            Event::PeerConnected { name } => assert_eq!(name, "phone"),
            _ => panic!(),
        }
    }

    #[test]
    fn tolerates_bracketed_timestamp_prefix() {
        let sw = parse(r#"[2026-05-19T12:34:56] NOTE: client "phone" has disconnected"#).unwrap();
        match sw {
            Event::PeerDisconnected { name } => assert_eq!(name, "phone"),
            _ => panic!(),
        }
    }

    #[test]
    fn returns_none_for_unrelated_lines() {
        assert!(parse("NOTE: started server").is_none());
        assert!(parse("DEBUG1: heartbeat sent").is_none());
        assert!(parse("").is_none());
        assert!(parse(r#"NOTE: client "x" did something else"#).is_none());
    }
}
