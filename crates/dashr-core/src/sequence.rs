//! A trace as a sequence diagram (DASHR-VIEW-002).
//!
//! Participants are the services, in order of first appearance, plus the
//! systems client spans call that send no trace of their own (a database,
//! a queue, DynamoDB). Every span becomes one message:
//!
//! * a span whose parent is in another service — a call from that service;
//! * a root span — a call from `caller`;
//! * a span whose parent never arrived — a call from `?`: the trace is
//!   broken there (lost context propagation), which is worth seeing;
//! * a client or producer span with no child in another service — a call
//!   to the peer it names;
//! * a client span whose callee did send spans — nothing: the callee's
//!   span draws the arrow;
//! * any other span — a self-message on its service: the custom spans
//!   that mark business steps.
//!
//! Synchronous calls get a return message carrying the duration.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;
use serde_json::Value;

use crate::model::{Span, SpanKind};
use crate::privacy::{Masker, Pseudonyms};

pub const CALLER: &str = "caller";
pub const BROKEN: &str = "?";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Call,
    Async,
    SelfCall,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Message {
    pub from: String,
    pub to: String,
    pub label: String,
    pub kind: MessageKind,
    pub span_id: String,
    pub start_ns: u64,
    pub end_ns: u64,
    pub error: bool,
    /// Nesting depth in the span tree, for indentation.
    pub depth: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sequence {
    pub participants: Vec<String>,
    pub messages: Vec<Message>,
    pub start_ns: u64,
}

pub fn build(spans: &[Span]) -> Sequence {
    let by_id: HashMap<&str, &Span> = spans.iter().map(|s| (s.span_id.as_str(), s)).collect();
    let mut children: HashMap<&str, Vec<&Span>> = HashMap::new();
    for span in spans {
        if let Some(parent) = span.parent_id.as_deref() {
            children.entry(parent).or_default().push(span);
        }
    }
    let depth = |span: &Span| {
        let mut depth = 0;
        let mut current = span.parent_id.as_deref();
        let mut seen = BTreeSet::new();
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            match by_id.get(id) {
                Some(parent) => {
                    depth += 1;
                    current = parent.parent_id.as_deref();
                }
                None => break,
            }
        }
        depth
    };
    let mut participants: Vec<String> = Vec::new();
    let add = |name: &str, participants: &mut Vec<String>| {
        if !participants.iter().any(|p| p == name) {
            participants.push(name.to_owned());
        }
    };
    let mut messages = Vec::new();
    for span in spans {
        let parent = span
            .parent_id
            .as_deref()
            .and_then(|id| by_id.get(id).copied());
        let asynchronous = matches!(span.kind, SpanKind::Consumer | SpanKind::Producer)
            || parent.is_some_and(|p| p.kind == SpanKind::Producer);
        let (from, to, kind) = match (span.parent_id.as_deref(), parent) {
            (None, _) => (CALLER.to_owned(), span.service.clone(), MessageKind::Call),
            (Some(_), None) => (BROKEN.to_owned(), span.service.clone(), MessageKind::Call),
            (Some(_), Some(parent)) if parent.service != span.service => (
                parent.service.clone(),
                span.service.clone(),
                if asynchronous {
                    MessageKind::Async
                } else {
                    MessageKind::Call
                },
            ),
            (Some(_), Some(_)) if matches!(span.kind, SpanKind::Client | SpanKind::Producer) => {
                let crosses = children
                    .get(span.span_id.as_str())
                    .is_some_and(|kids| kids.iter().any(|k| k.service != span.service));
                if crosses {
                    continue;
                }
                let peer = span.peer().unwrap_or_else(|| "external".into());
                let kind = if span.kind == SpanKind::Producer {
                    MessageKind::Async
                } else {
                    MessageKind::Call
                };
                (span.service.clone(), peer, kind)
            }
            _ => (
                span.service.clone(),
                span.service.clone(),
                MessageKind::SelfCall,
            ),
        };
        // A root client span (a test script calling an API) still calls its
        // peer; draw it from its own service.
        let (from, to, kind) =
            if from == CALLER && matches!(span.kind, SpanKind::Client | SpanKind::Producer) {
                let crosses = children
                    .get(span.span_id.as_str())
                    .is_some_and(|kids| kids.iter().any(|k| k.service != span.service));
                if crosses {
                    // Its callee's span will draw the call; show the caller as
                    // the client service.
                    add(&span.service, &mut participants);
                    continue;
                }
                (
                    span.service.clone(),
                    span.peer().unwrap_or_else(|| "external".into()),
                    kind,
                )
            } else {
                (from, to, kind)
            };
        // A callee named after its own service (X-Ray segments are) says
        // less than the call that reached it: use the caller's span name.
        let label = match parent {
            Some(p)
                if from != to
                    && span.name == span.service
                    && matches!(p.kind, SpanKind::Client | SpanKind::Producer) =>
            {
                p.name.clone()
            }
            _ => span.name.clone(),
        };
        add(&from, &mut participants);
        add(&to, &mut participants);
        messages.push(Message {
            from,
            to,
            label,
            kind,
            span_id: span.span_id.clone(),
            start_ns: span.start_ns,
            end_ns: span.end_ns,
            error: span.is_error(),
            depth: depth(span),
        });
    }
    // Callers first: `caller` and `?` lead, the rest keep first appearance.
    participants.sort_by_key(|p| match p.as_str() {
        CALLER => 0,
        BROKEN => 1,
        _ => 2,
    });
    Sequence {
        participants,
        messages,
        start_ns: spans.iter().map(|s| s.start_ns).min().unwrap_or(0),
    }
}

fn shown_attributes(span: &Span, masker: &Masker, pseudonyms: &mut Pseudonyms) -> String {
    let noisy = |key: &str| {
        key.starts_with("otel.")
            || key.starts_with("telemetry.")
            || key.starts_with("thread.")
            || key.starts_with("code.")
            || key == "span.kind"
            || key == "peer.service"
    };
    let masked: BTreeMap<String, Value> = masker.attributes(
        &span
            .attributes
            .iter()
            .filter(|(k, _)| !noisy(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        pseudonyms,
    );
    masked
        .iter()
        .map(|(k, v)| {
            format!(
                "{k}={}",
                v.as_str().map_or_else(|| v.to_string(), str::to_owned)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The sequence as text for the agent: one line per message, offsets from
/// the trace start, nesting by indentation, masked attributes.
pub fn text(spans: &[Span], masker: &Masker) -> String {
    let sequence = build(spans);
    let by_id: HashMap<&str, &Span> = spans.iter().map(|s| (s.span_id.as_str(), s)).collect();
    let mut pseudonyms = Pseudonyms::default();
    let mut out = String::new();
    for message in &sequence.messages {
        let offset = message.start_ns.saturating_sub(sequence.start_ns) as f64 / 1e6;
        let duration = message.end_ns.saturating_sub(message.start_ns) as f64 / 1e6;
        let arrow = match message.kind {
            MessageKind::Call => format!("{} -> {}", message.from, message.to),
            MessageKind::Async => format!("{} ~> {}", message.from, message.to),
            MessageKind::SelfCall => format!("{} ·", message.from),
        };
        let span = by_id.get(message.span_id.as_str());
        let label = masker.text(&message.label, &mut pseudonyms);
        let mut line = format!(
            "+{offset:>8.1}ms {}{arrow}: {label} [{duration:.1}ms]",
            "  ".repeat(message.depth)
        );
        if message.error {
            let reason = span
                .and_then(|s| s.status_message.as_deref())
                .map(|m| masker.text(m, &mut pseudonyms))
                .unwrap_or_else(|| "error".into());
            line.push_str(&format!(" ERROR: {reason}"));
        }
        if let Some(span) = span {
            let attributes = shown_attributes(span, masker, &mut pseudonyms);
            if !attributes.is_empty() {
                line.push_str(&format!("  {{{attributes}}}"));
            }
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;
    use crate::store::tests::span;
    use serde_json::json;

    fn trace() -> Vec<Span> {
        let mut root = span("a", "1", None, "orders-api", "POST /orders", 0, 100_000_000);
        root.kind = SpanKind::Server;
        let validate = span(
            "a",
            "2",
            Some("1"),
            "orders-api",
            "validate order",
            1_000_000,
            2_000_000,
        );
        let mut call = span(
            "a",
            "3",
            Some("1"),
            "orders-api",
            "PUT /stock",
            3_000_000,
            40_000_000,
        );
        call.kind = SpanKind::Client;
        let mut serve = span(
            "a",
            "4",
            Some("3"),
            "stock-api",
            "PUT /stock",
            4_000_000,
            39_000_000,
        );
        serve.kind = SpanKind::Server;
        serve.status = Status::Error;
        serve.status_message = Some("no stock for ann@example.com".into());
        let mut db = span(
            "a",
            "5",
            Some("4"),
            "stock-api",
            "UPDATE stock",
            5_000_000,
            6_000_000,
        );
        db.kind = SpanKind::Client;
        db.attributes
            .insert("db.system".into(), json!("postgresql"));
        let mut publish = span(
            "a",
            "6",
            Some("1"),
            "orders-api",
            "orders publish",
            50_000_000,
            51_000_000,
        );
        publish.kind = SpanKind::Producer;
        publish
            .attributes
            .insert("messaging.destination.name".into(), json!("orders"));
        let lost = span(
            "a",
            "7",
            Some("ff"),
            "mailer",
            "send",
            60_000_000,
            61_000_000,
        );
        vec![root, validate, call, serve, db, publish, lost]
    }

    #[test]
    fn spans_become_calls_self_messages_and_peers() {
        let sequence = build(&trace());
        assert_eq!(
            sequence.participants,
            [
                "caller",
                "?",
                "orders-api",
                "stock-api",
                "postgresql",
                "orders",
                "mailer"
            ]
        );
        let arrows: Vec<(String, String, MessageKind)> = sequence
            .messages
            .iter()
            .map(|m| (m.from.clone(), m.to.clone(), m.kind))
            .collect();
        assert_eq!(
            arrows,
            [
                ("caller".into(), "orders-api".into(), MessageKind::Call),
                (
                    "orders-api".into(),
                    "orders-api".into(),
                    MessageKind::SelfCall
                ),
                ("orders-api".into(), "stock-api".into(), MessageKind::Call),
                ("stock-api".into(), "postgresql".into(), MessageKind::Call),
                ("orders-api".into(), "orders".into(), MessageKind::Async),
                ("?".into(), "mailer".into(), MessageKind::Call),
            ]
        );
        assert!(
            sequence.messages[2].error,
            "the callee's error colours the call"
        );
        assert_eq!(sequence.messages[3].depth, 3);
    }

    #[test]
    fn segments_named_after_their_service_take_the_callers_name() {
        let mut call = span("a", "1", None, "sfn", "Invoke", 0, 10);
        call.kind = SpanKind::Client;
        let mut function = span("a", "2", Some("1"), "validate-fn", "validate-fn", 1, 9);
        function.kind = SpanKind::Server;
        let sequence = build(&[call, function]);
        assert_eq!(sequence.messages.last().unwrap().label, "Invoke");
    }

    #[test]
    fn text_for_the_agent_is_masked() {
        let text = text(&trace(), &Masker::default());
        assert!(
            text.contains(
                "orders-api -> stock-api: PUT /stock [35.0ms] ERROR: no stock for <email#1>"
            ),
            "{text}"
        );
        assert!(text.contains("orders-api ·: validate order"));
        assert!(text.contains("{db.system=postgresql}"));
        assert!(!text.contains("ann@example.com"));
    }
}
