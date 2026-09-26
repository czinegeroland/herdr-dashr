//! OTLP/HTTP JSON log payloads, for `dashr tail`.
//!
//! OTLP over HTTP accepts JSON (`application/json`) as well as protobuf, so
//! shipping a command's output needs no protobuf or gRPC dependency: each
//! line becomes one log record under a resource carrying `service.name`
//! (requirement DASHR-OTEL-003).

use serde_json::{Value, json};

/// Which stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }
}

/// One captured line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub time_unix_nano: u128,
    pub stream: Stream,
    pub body: String,
}

/// A terminal line as it should be stored: colour and cursor escapes
/// removed (so patterns match what the human reads), trailing `\r` dropped,
/// and at most [`MAX_LINE_BYTES`] long.
pub fn clean_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                // CSI: ESC [ parameters, final byte in @..~
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... BEL or ESC \
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
            continue;
        }
        if c == '\r' || (c.is_control() && c != '\t') {
            continue;
        }
        out.push(c);
    }
    if out.len() > MAX_LINE_BYTES {
        let mut end = MAX_LINE_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push('…');
    }
    out
}

/// Longest line body shipped; longer lines are cut.
pub const MAX_LINE_BYTES: usize = 16 * 1024;

/// OTel severity text and number guessed from a line's own words.
///
/// Many programs print their level; when a line names none, stdout is INFO
/// and stderr is left unspecified rather than assumed to be an error.
pub fn severity(line: &str, stream: Stream) -> (&'static str, u8) {
    let upper = line.to_ascii_uppercase();
    let has = |word: &str| {
        upper
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|token| token == word)
    };
    if has("FATAL") || has("PANIC") || has("CRITICAL") {
        ("FATAL", 21)
    } else if has("ERROR") || has("ERR") {
        ("ERROR", 17)
    } else if has("WARN") || has("WARNING") {
        ("WARN", 13)
    } else if has("DEBUG") {
        ("DEBUG", 5)
    } else if has("TRACE") {
        ("TRACE", 1)
    } else if has("INFO") || stream == Stream::Stdout {
        ("INFO", 9)
    } else {
        ("UNSPECIFIED", 0)
    }
}

/// The `/v1/logs` body for a batch of lines from one service.
pub fn logs_payload(service: &str, lines: &[LogLine]) -> Value {
    let records: Vec<Value> = lines
        .iter()
        .map(|line| {
            let (text, number) = severity(&line.body, line.stream);
            json!({
                "timeUnixNano": line.time_unix_nano.to_string(),
                "observedTimeUnixNano": line.time_unix_nano.to_string(),
                "severityText": text,
                "severityNumber": number,
                "body": {"stringValue": line.body},
                "attributes": [{"key": "log.iostream", "value": {"stringValue": line.stream.name()}}]
            })
        })
        .collect();
    json!({"resourceLogs": [{
        "resource": {"attributes": [
            {"key": "service.name", "value": {"stringValue": service}},
            {"key": "telemetry.sdk.name", "value": {"stringValue": "dashr-tail"}}
        ]},
        "scopeLogs": [{"scope": {"name": "dashr-tail"}, "logRecords": records}]
    }]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_cleaned_of_terminal_escapes() {
        assert_eq!(
            clean_line("\u{1b}[1;31mERROR\u{1b}[0m boom\r"),
            "ERROR boom"
        );
        assert_eq!(clean_line("\u{1b}]0;title\u{7}ok\ttab"), "ok\ttab");
        let long = "é".repeat(MAX_LINE_BYTES);
        let cleaned = clean_line(&long);
        assert!(cleaned.len() <= MAX_LINE_BYTES + '…'.len_utf8());
        assert!(cleaned.ends_with('…'));
    }

    #[test]
    fn severity_comes_from_the_line_then_the_stream() {
        assert_eq!(
            severity("2026 ERROR payment failed", Stream::Stdout).0,
            "ERROR"
        );
        assert_eq!(severity("[warn] slow query", Stream::Stdout).0, "WARN");
        assert_eq!(severity("level=debug msg=x", Stream::Stdout).0, "DEBUG");
        assert_eq!(severity("order created", Stream::Stdout).0, "INFO");
        assert_eq!(severity("order created", Stream::Stderr).0, "UNSPECIFIED");
        assert_eq!(
            severity("terror alert", Stream::Stdout).0,
            "INFO",
            "whole words only"
        );
    }

    #[test]
    fn payload_is_otlp_json_with_service_and_stream() {
        let payload = logs_payload(
            "checkout",
            &[LogLine {
                time_unix_nano: 1_790_000_000_000_000_000,
                stream: Stream::Stderr,
                body: "boom ERROR".into(),
            }],
        );
        let resource = &payload["resourceLogs"][0];
        assert_eq!(
            resource["resource"]["attributes"][0]["value"]["stringValue"],
            "checkout"
        );
        let record = &resource["scopeLogs"][0]["logRecords"][0];
        assert_eq!(record["timeUnixNano"], "1790000000000000000");
        assert_eq!(record["severityText"], "ERROR");
        assert_eq!(record["body"]["stringValue"], "boom ERROR");
        assert_eq!(record["attributes"][0]["value"]["stringValue"], "stderr");
    }
}
